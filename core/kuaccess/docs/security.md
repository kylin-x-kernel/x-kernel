# kuaccess — 安全与可靠性分析

## 信任模型

- 用户态地址、长度、字符串内容均不可信。
- `kuaccess` 负责把这些输入约束在当前进程用户地址空间内，并把失败显式返回给上层。

## 外部边界 / 攻击面

- 所有 `vm_load_string*()` 调用点。
- 所有经 `osvm::VirtMemIo` 落到 `Vm` 的读写请求。
- 访问窗口内触发的页错误处理。

## unsafe 代码清单

- `user_copy(...)`：
  依赖调用前的用户地址范围检查，以及当前线程 `accessing_user_memory` 标志保证 trap handler 只在受控窗口内接管。
- `user_atomic_cmpxchg_u32(...)`：
  依赖 4 字节对齐检查、用户地址范围检查，以及同一用户访问窗口与 exception-table fixup。

## 内存安全不变量

- `check_access()` 只允许访问用户空间有效区间。
- `atomic_cmpxchg_u32()` / `atomic_load_u32()` / `atomic_u32_eq()` 额外要求地址 4 字节对齐。
- `dispatch_irq_page_fault()` 仅在当前线程显式进入用户内存访问窗口时处理 fault。
- `kuaccess` 必须保留 `MmSpace::handle_page_fault()` 的 typed outcome 分类：
  只有 `Resolved` 和 retry-class outcome 可以转换为 trap handled；unmapped、
  permission denied、bus error、OOM、no-progress 和 generic failure 必须转换为
  user-copy / atomic-user 失败，而不是继续重试。
- 字符串 helper 只在成功读取完整字节流后再做 UTF-8 解释。

## 线程安全

- 不持有跨线程共享可变状态。
- 线程本地的访问窗口标志由当前线程对象维护，不允许跨线程复用。

## 威胁分析

- 越界地址：由 `check_access()` 拒绝。
- 缺页或未映射页：转交地址空间 typed 缺页处理；无法解析为 resolved/retry 时向上传播失败。
- 非 UTF-8 输入：映射为 `IllegalBytes`，避免把脏数据当路径或参数继续处理。

## 故障模式与影响分析（FMEA）

- current task 不是线程：`access_user_memory()` 直接 panic，暴露调用方上下文错误。
- 用户地址在访问过程中失效：返回 `MemError::NoAccess` 或等价 `KError`。
- 页错误被错误线程接管：依赖访问窗口标志避免。
- typed fault outcome 被错误压扁为 true：可能导致 user-copy 在不可恢复 fault 上反复重试；
  `fault_outcome_to_trap_result()` 集中维护 outcome 到 trap bool 的唯一映射。

## 故障管理

- 地址/映射问题走 `Result` 返回。
- 只有“线程上下文假设被破坏”这类内部不变量错误会 panic。

## 已知限制

- 当前 helper 只覆盖字符串装载与 `u32` 原子 cmpxchg；复杂结构体解析仍由各调用方自行组织。
- 原子原语目前仅提供 32-bit cmpxchg；更大宽度或其它 RMW 操作按需再加。
