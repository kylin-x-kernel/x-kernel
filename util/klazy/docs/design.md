# klazy — 设计文档

## 定位

`klazy` 提供兼容 `no_std` 的一次性初始化和延迟求值同步原语。这些原语是
x-kernel 中内核服务静态延迟初始化的基础构件（如 tracing、内存管理、TEE
服务、密码学模块）。

## 背景

`std::sync::Once` 和 `std::sync::LazyLock` 依赖标准库。x-kernel 运行在
裸机 `no_std` 环境中，无法使用这些类型。本模块提供等价功能，不依赖 `std`、
OS 线程或条件变量。

参考实现改编自 `spin::once` 和 `std::sync::SyncLazy`。

## 范围

涉及的源文件：

```
core/klazy/
├── src/
│   ├── lib.rs
│   ├── once.rs
│   └── lazy.rs
└── Cargo.toml
```

## 架构

两个类型，分层构建：

```
Lazy<T, F>  ──使用──>  Once<T>
```

| 组件 | 职责 |
|------|------|
| `Once<T>` | 线程安全的单元格，保证闭包仅执行一次。四状态原子状态机，持有初始化后的值 |
| `Lazy<T, F>` | 在 `Once<T>` 之上包装工厂函数 `F`，首次访问时通过 `Deref` 触发初始化。支持 `const` 构造，适合用作静态变量 |

## 状态机（Once）

```
                  call_once()
  Uninitialized ────────────> Initializing ────> Ready
       │                           │
       │                      panic!
       │                           │
       │                           v
       └─────────────────────  Failed
                                  │
     try_call_once()              │
     返回 Err，重置为              │
     Uninitialized                │
                                  v
                             poison: 所有
                             后续调用 panic
```

状态以 `u8` 判别值存储在 `AtomicU8` 中：

| 状态           | 值   | 含义                     |
|---------------|------|--------------------------|
| Uninitialized | 0x00 | 尚未开始初始化            |
| Initializing  | 0x01 | 某线程正在执行初始化闭包   |
| Ready         | 0x02 | 值已初始化，可读取         |
| Failed        | 0x03 | 初始化闭包 panic（已中毒） |

### 状态转换

| 从            | 到            | 触发条件                        |
|---------------|---------------|--------------------------------|
| Uninitialized | Initializing  | 首个线程赢得 CAS               |
| Initializing  | Ready         | 初始化闭包返回 Ok              |
| Initializing  | Failed        | 初始化闭包 panic（PanicGuard） |
| Initializing  | Uninitialized | `try_call_once` 返回 Err       |
| Failed        | —             | 所有后续调用 panic（中毒）      |

## 内存序

| 操作                          | 内存序     | 原因                                  |
|-------------------------------|-----------|---------------------------------------|
| 快速路径 `get()`              | `Acquire` | 与 writer 侧的 `Release` 配对          |
| CAS 竞争                      | `Acquire` | 成功和失败路径都需要建立 happens-before |
| store `Ready`                 | `Release` | 将初始化值发布给所有 reader             |
| store `Uninitialized`（出错时） | `Release` | 发布重置状态，重试线程能看到干净状态     |
| PanicGuard drop               | `SeqCst`  | 保守选择 — panic unwind 是边界场景      |

`is_completed()` 使用 `Acquire`，足以与 `Release` store 配对，保证后续
`get_unchecked()` 调用安全。

## 并发模型

- **自旋等待**：`wait()` 和 `poll()` 在状态为 `Initializing` 时自旋
  （`core::hint::spin_loop`），不使用阻塞原语。

- **快速路径**：`get()` 仅一次 `Acquire` load 加分支判断 — 初始化完成后
  零开销。

- **慢速路径**（`try_call_once_slow`）：CAS 循环竞争初始化权，正确协调
  并发调用者。

### 权衡：自旋 vs 阻塞

自旋等待适用于内核早期启动阶段和阻塞原语不可用的场景。对于执行时间较长的
初始化闭包，自旋线程会浪费 CPU。这是可接受的，因为：

1. x-kernel 中的初始化闭包通常很快（寄存器配置、静态分配）。
2. 这些原语首次使用时，调度器和条件变量尚不可用。

## Panic 与中毒

`PanicGuard` 在 unwind 时将单元格标记为 `Failed`。中毒后：

- `call_once()` / `try_call_once()` / `wait()` / `poll()` 均会 panic。
- `get()` 返回 `None`（不 panic — 允许读取中毒前状态，尽管新单元格无此状态）。

这与 `std::sync` 的中毒语义一致：panic 意味着状态可能已损坏，所有后续访问
被拒绝。

对于 `Lazy<T, F>`，工厂函数在首次 `force()` 时通过 `Cell::take()` 消费。
如果初始化闭包 panic，工厂函数丢失，`Lazy` 永久不可用（与
`std::sync::SyncLazy` 行为一致）。

## 设计决策

### 为什么不用 `spin::Once`

上游 `spin` crate 提供类似功能。我们将设计引入 `klazy` 是为了：

- 控制 API 范围（添加 `try_call_once`、`initialized()` 等）。
- 不受 `spin` 发布节奏影响，确保稳定性。
- 保持内核 crate 的依赖树最小。

### 为什么不用 `atomic_polyfill` / portable-atomics

直接使用 `core::sync::atomic`。在 `AtomicU8` 非 lock-free 的架构上需要
polyfill。当前支持的目标（x86_64、aarch64、riscv64、loongarch64）上
`AtomicU8` 均为 lock-free，无需额外依赖。

## Drop 行为

`Once<T>` 实现了 `Drop`：状态为 `Ready` 时，通过 `ptr::drop_in_place`
drop 内部 `T`。未初始化的单元格 drop 时不会触碰 `MaybeUninit` storage
（无 UB）。
