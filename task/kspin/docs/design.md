# kspin - 设计文档

## 定位

`kspin` 是 x-kernel 的内核自旋锁 crate。
它提供带 RAII guard 的 `SpinLock<G, T>`，
并通过 guard 类型在编译期选择进入临界区时是否关闭本地 IRQ、
是否关闭内核抢占。

目标读者是维护调度、IRQ、驱动和底层同步路径的开发者。

## 背景

内核运行在 `no_std` 环境，
很多路径不能睡眠或不能依赖阻塞式 mutex。
自旋锁适合短临界区，
但在内核中单纯的 atomic lock 不够：
同一 CPU 上的抢占或中断处理程序可能重入并访问同一数据，
导致死锁或数据竞争。

`kspin` 把“锁状态”和“进入临界区前的 CPU 本地保护”拆开：

- `SpinLock` 负责跨 CPU 互斥。
- guard 类型负责本地 IRQ / preemption 状态。

## 范围

涉及的源文件：

```text
task/kspin/
├── src/
│   ├── lib.rs                  # crate 文档、公开 re-export 和类型别名
│   ├── lock.rs                 # SpinLock 与 SpinLockGuard
│   ├── guard/
│   │   ├── mod.rs              # BaseGuard 与 KernelGuardIf
│   │   ├── types.rs            # NoOp/IrqSave/NoPreempt/NoPreemptIrqSave
│   │   └── arch/mod.rs         # karch IRQ save/restore 适配
│   └── tests.rs                # unittest 测试
├── README.md                   # crate-level rustdoc 内容
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```text
caller
  │
  │ lock()
  v
┌─────────────────────────────────────────────┐
│ SpinLock<G, T>                              │
│  marker: PhantomData<G>                     │
│  flag: AtomicBool       (feature = smp)     │
│  storage: UnsafeCell<T>                     │
└──────────────────┬──────────────────────────┘
                   │ G::acquire()
                   v
┌─────────────────────────────────────────────┐
│ BaseGuard implementation                    │
│  NoOp / NoPreempt / IrqSave                 │
│  NoPreemptIrqSave                           │
└──────────────────┬──────────────────────────┘
                   │ acquired
                   v
┌─────────────────────────────────────────────┐
│ SpinLockGuard<'_, G, T>                     │
│  guard_state: G::State                      │
│  ptr: *mut T                                │
│  flag_ref: &AtomicBool  (feature = smp)     │
└─────────────────────────────────────────────┘
```

| 组件 | 职责 |
|------|------|
| `SpinLock<G, T>` | 保存被保护数据和可选 SMP atomic 状态 |
| `SpinLockGuard` | 持锁期间提供 `Deref` / `DerefMut`，drop 时释放锁并恢复 guard 状态 |
| `BaseGuard` | 抽象进入/退出本地临界区的 acquire/release 协议 |
| `KernelGuardIf` | `preempt` feature 下由调度器提供 enable/disable preempt 接口 |
| `NoOp` | 不执行本地保护，适合调用方已关闭 IRQ/抢占的上下文 |
| `IrqSave` | 保存并关闭本地 IRQ，drop 时恢复 |
| `NoPreempt` | 关闭内核抢占，drop 时恢复 |
| `NoPreemptIrqSave` | 先关闭抢占再保存关闭 IRQ，释放时先恢复 IRQ 再恢复抢占 |

## 模块边界自审

`kspin` 的职责保持在“底层自旋锁与本地临界区 guard”：

- 不实现调度器逻辑，
  抢占开关只通过 `KernelGuardIf` 由 `ktask` 等上层提供。
- 不直接操作架构寄存器，
  IRQ save/restore 经 `karch` 统一封装。
- 不封装睡眠等待、条件变量、读写锁或 semaphore，
  这些由 `ksync` 等更高层同步 crate 承担。
- 不依赖 `ktask`、`kservices`、`ktracing` 或驱动子系统，
  依赖方向是这些上层 crate 使用 `kspin`。

公开 API 自审结果：

- 保留 `SpinLock`、`SpinLockGuard`、`BaseGuard`、`KernelGuardIf`
  和 `SpinRaw` / `SpinNoPreempt` / `SpinNoIrq` 类型别名。
- `guard::arch` 是私有模块，
  IRQ save/restore helper 没有跨 crate 暴露。
- 未接入模块树的旧重复实现 `src/base.rs` 已删除，
  避免出现第二套 unsafe spinlock API 和重复审计面。
- `force_unlock` 保持 `unsafe fn`，
  只服务 `lock_api::RawMutex` 这类无法持有 RAII guard 的适配场景。

## 状态机

### SMP lock 状态

```text
Unlocked(flag = false)
   │ compare_exchange(false -> true, Acquire)
   v
Locked(flag = true)
   │ SpinLockGuard::drop()
   │ store(false, Release)
   v
Unlocked
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Unlocked | Locked | `lock` 或 `try_lock` CAS 成功 |
| Locked | Locked | `lock` CAS 失败后自旋等待 |
| Locked | Unlocked | `SpinLockGuard::drop` 执行 Release store |
| Locked | Unlocked | `unsafe force_unlock` 在调用者证明持锁时强制释放 |

### 非 SMP lock 状态

```text
SingleCpu
   │ lock / try_lock
   v
GuardedByLocalGuard
   │ drop
   v
SingleCpu
```

未启用 `smp` feature 时，
atomic flag 被编译移除。
互斥依赖 guard 类型提供的本地不可重入保证，
例如关闭抢占或 IRQ。

### `NoPreemptIrqSave` 状态顺序

```text
Normal
  │ disable_preempt
  v
PreemptDisabled
  │ save_disable_irq
  v
PreemptDisabledIrqDisabled
  │ restore_irq
  v
PreemptDisabled
  │ enable_preempt
  v
Normal
```

该顺序避免在 IRQ 已恢复但抢占状态尚未一致时被调度出去。

## 算法流程

### `lock`

1. 调用 `G::acquire()`，进入本地临界区并保存 guard 状态。
2. 启用 `smp` 时，通过 `compare_exchange_weak(false, true, Acquire, Relaxed)` 尝试抢锁。
3. CAS 失败时，在 `is_locked()` 为真期间执行 `spin_loop()`。
4. 成功后从 `UnsafeCell<T>` 取出指针，
   构造 `SpinLockGuard`。
5. 调用者通过 guard 的 `Deref` / `DerefMut` 访问数据。

### `try_lock`

1. 先执行 `G::acquire()`。
2. 启用 `smp` 时用强 CAS 尝试把 flag 从 false 改为 true。
3. 成功则返回 guard。
4. 失败则立即执行 `G::release(guard_state)`，
   恢复本地 IRQ / preemption 状态并返回 `None`。

### `SpinLockGuard::drop`

1. 启用 `smp` 时以 `Release` store 把 flag 写回 false。
2. 调用 `G::release(guard_state)` 恢复本地状态。

释放顺序保证其他 CPU 在 Acquire CAS 成功后能看到临界区内的写入。

### `get_mut`

`get_mut(&mut self)` 依赖 Rust 独占借用，
不需要设置 atomic flag。
因为调用者持有 `&mut SpinLock`，
不可能同时存在任何 guard 或其他引用。

### `force_unlock`

`force_unlock` 只清除 SMP flag，
不会恢复 guard 状态，
也不会 drop 被泄漏的 guard。
它仅用于 FFI 或测试等 RAII 无法表达的边界，
调用者必须证明当前执行路径确实持有该锁。

## 并发模型

锁策略：

- `smp` feature 开启时，跨 CPU 互斥由 `AtomicBool` 提供。
- `smp` feature 关闭时，不存在跨 CPU 竞争，
  互斥依赖 guard 阻止同 CPU 抢占或 IRQ 重入。
- `SpinRaw` 不提供本地保护，
  只能在调用方已经保证不可重入的上下文使用。
- `SpinNoIrq` 是默认最保守选择，
  可覆盖普通任务上下文和 IRQ 上下文访问同一数据的场景。

内存序：

- 成功 acquire 使用 `Ordering::Acquire`。
- unlock 使用 `Ordering::Release`。
- 自旋观察 `is_locked` 使用 `Ordering::Relaxed`，
  只作为等待提示，不承载同步。

## Cargo Features

| Feature | 作用 |
|---------|------|
| `smp` | 启用 `AtomicBool` lock flag，实现跨 CPU 互斥 |
| `preempt` | 通过 `KernelGuardIf` 调用调度器的抢占开关 |

未启用 `preempt` 时，
`NoPreempt` 和 `NoPreemptIrqSave` 的抢占开关部分会被编译为空操作。

## 设计决策

### 为何 guard 是类型参数

guard 类型参数把临界区策略编码进类型：
同一个 `SpinLock` 实例的本地保护方式不能在运行时被误改。
调用点从类型别名即可看出锁适合的上下文。

### 为何单核移除 atomic flag

在无 `smp` 配置下没有其他 CPU 竞争。
保留 atomic flag 只会增加指令和存储开销。
互斥问题退化为本 CPU 是否会被抢占或中断重入，
由 guard 负责。

### 为何 `try_lock` 失败也要先 acquire guard

如果不先关闭本地抢占或 IRQ，
在检查 lock flag 和返回之间可能被本地中断/抢占路径重入。
先 acquire guard 可以让成功和失败路径都处于一致的本地临界区模型。
失败时立即 release，避免泄露 IRQ/preemption 状态。

### 为何存在 `force_unlock`

少数 FFI 或测试场景无法让 RAII guard 正常 drop。
`force_unlock` 保留逃生口，
但以 `unsafe fn` 暴露，
并在安全文档中要求调用者证明当前路径持锁。

## Drop / 资源释放

`SpinLock` 没有自定义 `Drop`。
当 lock 被销毁时，
被保护的 `T` 按普通 Rust 所有权规则 drop。

`SpinLockGuard::drop` 是核心释放路径：
它释放 SMP flag 并恢复 guard 状态。
若 guard 被 `mem::forget`，
锁保持 locked 状态，
除非调用者使用 `unsafe force_unlock`。
