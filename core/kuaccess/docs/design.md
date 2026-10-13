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
- `ut-tests/copy_tests.rs`（用户复制回归测试）

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
- 普通 `osvm` 复制入口要求可睡眠的线程上下文，不自行关闭 IRQ。页错误通过
  exception-table fixup 退出汇编后，在调用任务中完成回填；文件页回填允许等待块 I/O。
- 原子用户访问的 trap handler 依赖当前线程的 `accessing_user_memory` 标志，
  只在该窗口内接管页错误。

## 算法流程

### 用户内存访问

1. 调用方通过 `osvm` 指针包装或 `vm_load_string*()` 发起访问。
2. `Vm` 先做用户地址范围检查。
3. `copy_user_pages()` 按用户地址的 4 KiB 页边界拆分复制，每段先调用 `user_copy`。
4. 普通复制不打开 trap 内回填窗口。缺页时通过 exception-table fixup 返回，
   随后在调用任务中调用当前进程的 `MmSpace::handle_page_fault()`。这样不会在
   IRQ 被屏蔽的异常上下文里等待 IRQ 驱动的磁盘完成。
5. 使用单一进度循环，每轮先尝试复制当前页片段：成功则推进 offset 并重置
   当前页的缺页次数；失败则最多处理一次缺页，再进入下一轮复制。
   `Resolved`、`Retry` 和 `CowConflictRetry` 都允许重试复制；
   unmapped、permission、bus、OOM、no-progress 和 generic failure 返回 `NoAccess`。
6. `Resolved` 后复制仍失败时继续处理当前页缺页，允许共享文件的
   “未映射 → 只读 PTE → 可写 PTE”分阶段完成。每个用户页最多调用缺页处理
   16 次，`Resolved` 和 retry-class outcome 共用该预算，下一页重新计数。
   预算耗尽后复制仍失败则返回 `NoAccess`，防止后端反复报告成功或重试却
   无实际进展时无限循环。该上限是防止无限重试的策略，并非通过次数判断 PTE 进展；
   持续并发修改映射也可能耗尽预算并导致失败。
   已映射页不需要取得地址空间锁；成功返回保证全部输出已初始化。

### 用户态原子访问

1. 校验 4 字节对齐与用户地址范围。
2. faultable 形式在 `access_user_memory()` 窗口内调用架构原语，允许 MM 处理
   可恢复缺页；nofault 形式直接依赖 exception-table fixup 返回失败。
3. load 使用自然对齐的只读 32-bit load；nofault cmpxchg 的 exclusive/atomic
   序列在 `IrqSave` 保护下执行。
4. 成功时返回 observed value 或 `(exchanged, observed)`；fault 映射为
   `MemError::NoAccess`。

这些原语供 futex / robust-list 等路径对用户态 futex word 做无 TOCTOU 的读取
或更新。只读 load 不要求映射可写，也不会触发 COW。

### 字符串装载

1. 从用户地址读取字节向量或 NUL 终止字节流。
2. 做 UTF-8 校验。
3. 失败返回 `IllegalBytes`。

## 并发模型

- `Vm` 不使用 `IrqSave`，普通复制的 fault 慢路径与磁盘读取一样要求可睡眠。
  nofault 原子访问仍保留其局部 `IrqSave` 保护。
- 不维护全局共享状态；真正的并发控制由线程状态和地址空间锁负责。

## 设计决策

- 用户字符串装载留在 `kuaccess`，而不是 syscall crate，因为它属于“如何安全访问用户内存”这一职责，不属于某个单独 syscall 族。
- 只保留字符串级 helper，不在这里扩张为新的通用用户态参数解析层。
- trap handler 的外部 ABI 仍是 `bool`，但这个 bool 现在只是架构 trap 分发的适配结果；
  MM 语义来自 `PageFaultOutcome`，避免 `kuaccess` 重新定义缺页分类。

## 回归验证

`ut-tests/copy_tests.rs` 使用要求 `PreparedTaskWait` 可用的按需映射后端，验证普通
复制的缺页处理发生在异常外的可睡眠任务上下文，并覆盖跨页读、首次写缺页、
只读保护和撤销映射。分阶段写缺页测试跨越多个页面并超过单页恢复预算，验证同一页连续缺页后
复制结果完整；无进展测试覆盖反复报告成功、反复报告 COW retry 的有界退出，
以及显式 `NoProgress` 的立即失败。测试不依赖真实磁盘或 IRQ 的时序。
