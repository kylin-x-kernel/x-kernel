# kspin - 安全与可靠性分析

## 概述

`kspin` 是底层同步原语，
包含 `UnsafeCell` 解引用、裸指针 guard、`Send`/`Sync` unsafe impl
以及 `force_unlock` 这样的显式 unsafe API。
如果锁获取、释放、本地 IRQ/抢占保护或内存序错误，
可能导致数据竞争、死锁、IRQ 状态泄露或跨 CPU 可见性错误。

## 信任模型

```text
kernel subsystem caller
   │
   │ safe API:
   │   SpinLock::{new,lock,try_lock,is_locked,get_mut,into_inner}
   │   SpinLockGuard::{Deref,DerefMut,Drop}
   │
   │ unsafe API:
   │   SpinLock::force_unlock
   v
┌─────────────────────────────────────────────┐
│ kspin                                      │
│                                             │
│ unsafe boundary                            │
│  ├─ UnsafeCell<T> -> *mut T -> &/&mut T    │
│  ├─ unsafe impl Send/Sync                  │
│  ├─ AtomicBool Acquire/Release protocol    │
│  └─ force_unlock caller contract           │
└──────────────────┬──────────────────────────┘
                   │ BaseGuard::acquire/release
                   v
karch IRQ state / ktask preemption interface
```

- safe API 调用者信任 `kspin` 在 guard 生命周期内提供唯一 mutable access。
- `kspin` 信任 `BaseGuard` 实现的 acquire/release 成对且可嵌套语义正确。
- `kspin` 信任 `KernelGuardIf` 的 preemption 开关不会睡眠或破坏当前 CPU 状态。
- `force_unlock` 调用者必须自行证明当前执行路径确实持有该锁。

## unsafe 代码清单

### 1. `SpinLock<G, T>` 的 `Sync` 实现

位置：`src/lock.rs`

```rust
unsafe impl<G: BaseGuard, T: ?Sized + Send> Sync for SpinLock<G, T> {}
```

不变量：

- 共享访问 `SpinLock` 时，
  内部 `T` 只能通过持锁的 `SpinLockGuard` 被引用。
- 启用 `smp` 时，
  `AtomicBool` flag 对 guard 创建进行跨 CPU 串行化。
- 未启用 `smp` 时，
  调用者选择的 guard 必须阻止同 CPU 重入。

为何安全：

- `lock` 和 `try_lock` 只有在 acquire 成功后才从 `UnsafeCell` 创建数据指针。
- `SpinLockGuard::drop` 释放 flag 并恢复 guard 状态。
- `T: Send` 与 `std::sync::Mutex` 的共享条件一致。

### 2. `SpinLock<G, T>` 的 `Send` 实现

位置：`src/lock.rs`

```rust
unsafe impl<G: BaseGuard, T: ?Sized + Send> Send for SpinLock<G, T> {}
```

不变量：

- 移动锁时不会同时保留旧位置的有效访问入口。
- 被保护数据可跨线程转移所有权。

为何安全：

- Rust 所有权移动保证旧 `SpinLock` 位置不可再被 safe code 使用。
- 移动后访问仍由同一 guard 和 atomic 协议保护。

### 3. `lock` 成功后从 `UnsafeCell` 创建指针

位置：`src/lock.rs`

```rust
ptr: unsafe { &mut *self.storage.get() },
```

不变量：

- 当前 guard 已经执行 `G::acquire()`。
- 启用 `smp` 时当前 CPU 已成功把 flag 从 false 改为 true。
- guard drop 前不会创建另一个 mutable reference。

为何安全：

- 成功持锁后，
  `SpinLockGuard` 是访问 `T` 的唯一入口。
- 指针只保存在 guard 中，
  由 guard 生命周期约束引用。

### 4. `try_lock` 成功后从 `UnsafeCell` 创建指针

位置：`src/lock.rs`

不变量与 `lock` 相同，
区别是 `try_lock` 使用强 CAS。
失败路径会调用 `G::release(guard_state)`，
不创建数据指针。

### 5. `get_mut`

位置：`src/lock.rs`

```rust
unsafe { &mut *self.storage.get() }
```

不变量：

- 调用者持有 `&mut SpinLock<G, T>`。
- Rust 独占借用保证不存在其他 guard 或共享引用。

为何安全：

- 不需要 runtime lock；
  编译期独占借用已经证明没有并发访问。

### 6. `SpinLockGuard::deref`

位置：`src/lock.rs`

```rust
unsafe { &*self.ptr }
```

不变量：

- `ptr` 来自成功持锁后的 `UnsafeCell`。
- 返回引用不能超过 guard 生命周期。

为何安全：

- `Deref` 借用 `&self`，
  共享引用只在 guard 仍持锁期间存在。

### 7. `SpinLockGuard::deref_mut`

位置：`src/lock.rs`

```rust
unsafe { &mut *self.ptr }
```

不变量：

- 当前 guard 以 `&mut self` 被独占借用。
- guard 仍持有锁。

为何安全：

- `&mut self` 防止同一 guard 同时创建多个 mutable reference。
- 其他 guard 被 lock 协议排除。

### 8. `SpinLock::force_unlock`

位置：`src/lock.rs`

```rust
pub unsafe fn force_unlock(&self)
```

不变量：

- 当前执行路径已经持有该锁。
- 调用后不会继续使用被泄漏 guard 创建出的引用。
- 调用者负责处理 guard 状态，
  因为 `force_unlock` 只清除 SMP flag，不恢复 IRQ/preemption state。

为何安全：

- 该函数本身仅执行 Release store。
- 安全性完全依赖调用者契约，
  因此必须保持 `unsafe fn`。

外部调用者审查：

- `core/ktracing::TraceRawLock` 实现 `lock_api::RawMutex` 时调用该 API。
  其 `lock` / `try_lock` 在成功后 `mem::forget` guard，
  `unlock` 由 `lock_api` 契约保证只在持锁后调用。
  调用点已补充 `SAFETY:` 注释。

### 9. `tests.rs` fake guard counter

位置：`src/tests.rs`

测试中的 `static mut IRQ_CNT` 用于验证 fake guard acquire/release 是否配对。
这些 unsafe 访问只存在于 `#[cfg(unittest)]` 测试代码，
并已在每个 unsafe 块前注明测试前提。

## 内存安全不变量

1. **guard 唯一访问**：
   任何 `&T` / `&mut T` 都必须从 `SpinLockGuard` 派生。
2. **flag 与 pointer 创建绑定**：
   启用 `smp` 时，
   只有成功 CAS 后才能创建 `ptr`。
3. **unlock 释放写入**：
   guard drop 使用 `Release` store，
   下一个持锁者通过 `Acquire` CAS 观察临界区写入。
4. **失败路径恢复 guard 状态**：
   `try_lock` 失败必须调用 `G::release`。
5. **`force_unlock` 不恢复本地 guard**：
   调用者不得把它当作普通 unlock 使用。
6. **`SpinRaw` 需要外部不可重入保证**：
   `NoOp` 不关闭抢占或 IRQ。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `SpinLock<G, T>` | unsafe impl when `T: Send` | unsafe impl when `T: Send`，依赖 guard + atomic 协议 |
| `SpinLockGuard<'_, G, T>` | 不显式实现；由字段决定 | guard 持有裸指针，不应跨线程共享访问 |
| `SpinRwLock<G, T>` | unsafe impl when `T: Send` | unsafe impl when `T: Send + Sync`，读 guard 可并发暴露 `&T` |
| `SpinRwLockReadGuard<'_, G, T>` | 不显式实现；由字段决定 | 通过 lock 的 reader 协议共享访问 |
| `SpinRwLockWriteGuard<'_, G, T>` | 不显式实现；由字段决定 | 写 guard 持有独占访问，不应跨线程共享可变访问 |
| `NoOp` | zero-sized | zero-sized |
| `IrqSave` | 保存 IRQ flags | 只应在当前 CPU 上 acquire/release |
| `NoPreempt` | zero-sized | 依赖 `KernelGuardIf` |
| `NoPreemptIrqSave` | 保存 IRQ flags | 依赖 `KernelGuardIf` + `karch` IRQ restore |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 未持锁创建 `&mut T` 导致数据竞争 | 高 | `UnsafeCell` 解引用在 CAS 前发生 | 代码只在成功获取 guard 后创建指针；`get_mut` 依赖 `&mut self` |
| T-02 | `SpinRaw` 在可抢占或 IRQ 可重入上下文使用 | 高 | 调用方误选 `SpinRaw` 保护 IRQ 共享数据 | 类型别名文档明确限制；默认推荐 `SpinNoIrq` |
| T-03 | `try_lock` 失败泄露 IRQ/preemption 禁用状态 | 高 | 失败路径忘记 `G::release` | 当前失败路径立即 release；单元测试覆盖 fake guard 恢复 |
| T-04 | unlock 内存序过弱导致写入不可见 | 高 | Release/Acquire 协议被降级 | unlock 使用 Release；成功 lock 使用 Acquire |
| T-05 | guard 被 `mem::forget` 后锁永久保持 locked | 中 | 调用方泄漏 guard | RAII 正常路径自动释放；`force_unlock` 仅作为 unsafe 逃生口 |
| T-06 | `force_unlock` 被非持锁者调用 | 高 | FFI、`lock_api` adapter 或测试误用 unsafe API | `force_unlock` 为 unsafe fn；已审查 `TraceRawLock` 和 unittest 调用点 |
| T-07 | `BaseGuard` acquire/release 顺序错误 | 高 | 新增 guard 未正确保存/恢复 IRQ 或抢占 | guard trait 集中抽象；`NoPreemptIrqSave` 固定先关抢占、再关 IRQ，释放反序 |
| T-08 | Debug 输出在持锁时递归尝试 lock | 低 | 对已锁对象格式化 | `Debug` 使用 `try_lock`，失败输出 `<locked>` |
| T-09 | 长临界区导致 CPU 自旋占用 | 中 | 持锁执行阻塞、I/O 或复杂循环 | 文档限定自旋锁用于短临界区；调用方审计热点 |
| T-10 | 未启用 `preempt` feature 却依赖 `NoPreempt` | 中 | 配置错误 | feature 表说明；平台 defconfig 需匹配调度模型 |
| T-11 | `SpinRwLock` read-to-write upgrade 自锁 | 中 | 持有 read guard 时尝试获取 write guard | 文档明确不支持 upgrade；调用方必须释放 read guard 后进入写事务 |
| T-12 | `SpinRwLock` reader count 溢出 | 中 | 极端递归或泄漏 read guard 后继续获取 read lock | reader count 达上限时 panic，避免覆盖 writer bit |
| T-13 | `SpinRwLock` writer 饥饿 | 中 | 持续新 reader 在 writer 等待期间进入 | 当前实现是 reader-preferred 简单 spin rwlock，只用于短临界区；需要公平性时另行设计 queued rwlock |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 死锁 | 同一上下文重复获取同一 non-reentrant 锁 | 当前 CPU 自旋 | 可能全系统卡死 | 1 | 避免递归锁；`lock` 文档标注可能死锁 |
| F-02 | IRQ 状态未恢复 | guard release 漏调或 guard 泄漏 | 本 CPU IRQ 关闭 | 定时器和设备中断停滞 | 1 | RAII drop；`try_lock` 失败释放；测试覆盖 fake guard |
| F-03 | 抢占状态未恢复 | `KernelGuardIf` 实现错误 | 当前任务不可抢占 | 调度延迟或卡死 | 2 | `preempt` feature 下由调度器集中实现接口 |
| F-04 | 自旋时间过长 | 临界区过大或持锁者卡死 | CPU 占用升高 | 系统延迟上升 | 2 | 仅用于短临界区；避免持锁 I/O |
| F-05 | `force_unlock` 后继续使用泄漏 guard | unsafe 调用者破坏契约 | 两个 guard 同时访问数据 | 数据竞争或内存破坏 | 1 | 保持 unsafe API；审计所有调用点 |
| F-06 | 单核配置下误以为有跨 CPU 互斥 | `smp` feature 未启用但运行在多核 | 无 atomic flag | 数据竞争 | 1 | defconfig 必须与 CPU 拓扑一致 |
| F-07 | Debug 输出隐藏锁内数据 | `try_lock` 失败 | 只能看到 `<locked>` | 调试信息降级 | 4 | 避免 Debug 阻塞或递归死锁 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- `lock` 不返回错误；
  竞争时持续自旋。
- `try_lock` 用 `Option` 表示成功或失败，
  失败时已经恢复 guard 状态。
- `force_unlock` 不做 runtime 检查，
  误用属于 unsafe 契约违反。
- 本 crate 不记录日志，
  避免在临界区和低层同步路径引入额外依赖。

## 隐私分析

`kspin` 不直接处理用户数据。
它保护的 `T` 可能包含任意内核或用户相关数据，
但该 crate 不读取、不复制、不打印 `T`，
除了 `Debug` 实现会在调用者显式格式化且成功取锁时格式化内部数据。

## 已知限制

1. **非公平锁**：
   当前实现没有队列或优先级继承。
2. **无死锁检测**：
   重入同一锁会自旋或死锁。
3. **无抢占 feature 时 `NoPreempt` 为空操作**：
   配置必须与调度模型一致。
4. **`force_unlock` 只释放 atomic flag**：
   不恢复 IRQ/preemption guard state。
5. **自旋锁不适合长临界区**：
   长时间持锁会放大中断延迟和 CPU 占用。

## 其它说明（模板章节）

| 章节 | 说明 |
|------|------|
| 基线 | 以本仓库 `docs/templates/module-docs-guide.md` 及 `AGENTS.md` 为准 |
| 冗余设计 | 无 |
| 过载控制 | 无；调用方必须保持临界区短 |
| 人因差错 | 无直接用户交互 |
| 故障预测预防 | 无 |
| 升级不中断业务 | 无 |

## 审计清单

修改 `kspin` 时需验证：

- [ ] 每个新增 unsafe 块、unsafe impl 或 unsafe API 都有前置 `SAFETY:` 注释。
- [ ] `try_lock` 失败路径恢复 guard 状态。
- [ ] lock/unlock 的 Acquire/Release 内存序不被削弱。
- [ ] 新增 guard 的 acquire/release 顺序成对且可审计。
- [ ] `SpinRaw` 的新调用点已经有外部 IRQ/抢占不可重入保证。
- [ ] 不在持有自旋锁期间执行可能睡眠、阻塞 I/O 或长时间循环的操作。
- [ ] defconfig 的 `smp` / `preempt` feature 与实际平台模型一致。
