# ktypes — 安全与可靠性分析

## 概述

`ktypes` 是 x-kernel 中广泛使用的底层同步原语，负责内核服务的一次性初始化。
其中包含 `unsafe` 代码，涉及内部可变性和原子状态转换。不正确的使用或不变量
破损将导致未定义行为。

## 信任模型

```
调用者（内核子系统）
   │
   │ safe API: call_once, get, wait, poll, is_completed
   │
   v
┌──────────────────────────────────┐
│  Once<T> / Lazy<T, F>           │
│                                  │
│  ┌── unsafe 边界 ──────────────┐ │
│  │ AtomicStatus::new_unchecked │ │
│  │ get_value_unchecked         │ │
│  │ as_mut_ptr                  │ │
│  │ MaybeUninit write/read      │ │
│  └─────────────────────────────┘ │
└──────────────────────────────────┘
```

- **safe API 调用者**信任 `ktypes` 正确维护其不变量。
- **unsafe API 调用者**（`get_unchecked`、`get_mut_unchecked`、
  `into_inner_unchecked`、`as_mut_ptr`）需自行证明初始化已完成。

## unsafe 代码清单

### 1. `AtomicStatus::new_unchecked`（`once.rs:89`）

```rust
unsafe fn new_unchecked(inner: u8) -> Self {
    core::mem::transmute(inner)
}
```

**不变量**：`u8` 值必须是有效的 `Status` 判别值（0x00–0x03）。

**为何安全**：仅从 `AtomicStatus::load` 和 `compare_exchange` 调用，
两者获取的 `u8` 均源自原子操作，而原子中存储的值最初都是合法的
`Status` 变体。`AtomicStatus` 封装阻止了任意 `u8` 值被写入。

### 2. `Once::get_value_unchecked`（`once.rs:388`）

```rust
unsafe fn get_value_unchecked(&self) -> &T {
    &*(*self.storage.get()).as_ptr()
}
```

**不变量**：状态必须为 `Ready`（值已初始化）。

**调用者**：
- `get()` — 由 `Acquire` load 检查 `Status::Ready` 保护。
- `poll()` — 由状态匹配 `Status::Ready` 保护。
- `try_call_once_slow` — 在 `Release` store `Ready` 之后调用。
- `get_unchecked()` — 调用者负责（debug 构建中有 `debug_assert`）。

### 3. `Once::try_call_once_slow` value write（`once.rs:287`）

```rust
let storage_ptr = (*self.storage.get()).as_mut_ptr();
storage_ptr.write(initialized_value);
```

**不变量**：调用者持有独占 write 权限（CAS 成功转为 `Initializing`），
此时无并发 reader 能观察到 `Ready` 状态。

### 4. `Once::as_mut_ptr`（`once.rs:382`）

```rust
pub fn as_mut_ptr(&self) -> *mut T
```

**不变量**：初始化前 read 此指针是 UB。write 可用于 FFI 互操作，
调用者承担全部责任。

### 5. `Once::Drop`（`once.rs:517`）

```rust
core::ptr::drop_in_place((*self.storage.get()).as_mut_ptr());
```

**不变量**：仅在状态为 `Ready` 时调用。独占 `&mut self` 保证无并发访问。

## 内存安全不变量

以下不变量必须在任何时候都成立：

1. **Single writer**：只有成功 CAS（从 `Uninitialized` 到 `Initializing`）的
   线程才能 write `storage`。

2. **Ready 前不可 read**：在状态以 `Release` 内存序 store 为 `Ready` 之前，
   `storage` 不得被当作 `T` read。

3. **AtomicStatus 有效性**：底层 `AtomicU8` 必须始终包含有效的 `Status`
   判别值。通过将 `AtomicStatus` 作为唯一接口并仅接受 `Status` 值（而非
   原始 `u8`）来保证。

4. **无 double-drop**：`Drop` 仅在状态为 `Ready` 时调用 `drop_in_place`。
   `try_call_once` 错误路径不会 drop 从未 write 的值。

## 线程安全

| 类型           | `Send` 条件            | `Sync` 条件          |
|---------------|------------------------|----------------------|
| `Once<T>`     | `T: Send`              | `T: Send + Sync`    |
| `Lazy<T, F>`  | 自动（字段均为 `Send`） | `Once<T>: Sync`     |

`Lazy` 不要求 `F: Sync`，因为工厂函数在 `Once` 同步机制保护下通过
`Cell::take()` 消费 — 只有一个线程会 read `F`。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 对未初始化 `Once` 调用 `get_unchecked()` 返回未初始化内存引用 | 高 | 调用者在未确认 `is_completed()` 前直接调用 `get_unchecked()` | debug 构建中 `debug_assert` 检测；release 构建依赖调用者正确使用 |
| T-02 | 通过 `as_mut_ptr()` 获取指针后，在初始化前 read | 高 | 调用者未遵守 `as_mut_ptr()` 文档约束 | 文档标注 `# Safety`，调用者承担证明责任；无运行时检查 |
| T-03 | 初始化闭包中 panic 导致 `PanicGuard` 将状态标记为 `Failed`，后续所有调用永久 panic | 中 | 闭包包含可能 panic 的代码（如越界访问、除零） | `try_call_once` 提供不 panic 的替代路径，错误时 reset 为 `Uninitialized` 可重试 |
| T-04 | 自旋等待无超时，初始化线程被抢占时自旋线程浪费 CPU | 低 | 初始化闭包执行时间过长，或调度器延迟高 | 仅用于快速初始化场景；文档约束不建议在热路径使用 |
| T-05 | `AtomicU8` 在目标架构上非 lock-free，内部可能使用全局锁 | 低 | 在不支持 native `AtomicU8` 的架构上运行 | 当前所有支持目标（x86_64/aarch64/riscv64/loongarch64）均为 lock-free |
| T-06 | `Lazy` 工厂函数被 `Cell::take()` 消费后，二次 `force()` panic | 中 | `force()` 首次因 panic 失败后再次调用 | 由 `Once` 中毒机制保护，二次调用直接 panic（符合 `std::sync` 语义） |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `get()` 对未初始化单元格返回 `None`，调用者未处理 | 调用时序错误，初始化尚未完成 | 依赖该值的功能跳过 | 使用该值的子系统功能缺失 | 3 | 返回 `Option` 强制调用者处理；建议使用 `wait()` 或 `call_once()` |
| F-02 | 初始化闭包 panic，单元格永久中毒 | 闭包内存在 bug（越界、断言失败等） | 该 `Once` 永不可用 | 依赖该 `Once` 的内核服务无法启动 | 2 | `PanicGuard` 标记 `Failed` 防止 UB；`try_call_once` 提供可重试路径 |
| F-03 | 多线程竞争 `call_once`，CAS 失败的线程自旋等待 | 正常并发场景 | 自旋线程 CPU 占用 | 短暂 CPU 浪费，无功能影响 | 4 | `spin_loop` 提示 CPU pause；初始化完成后立即返回 |
| F-04 | `Once` 被 `mem::forget` 泄漏，内部 `T` 不被 drop | 外部代码误用 | `T` 占用的资源不释放 | 内存或资源泄漏 | 3 | Rust 生态已知问题，不构成内存安全问题 |
| F-05 | `try_call_once` 错误路径重置为 `Uninitialized` 后，另一线程立即 CAS 成功 | 错误恢复后立即重试 | 初始化可能被重复尝试 | 无功能影响（CAS 保证单次执行） | 4 | 正常行为，设计如此 |
| F-06 | `AtomicStatus` 存入非法 `u8` 值导致 `transmute` 产生无效 `Status` | unsafe 代码被外部修改破坏 | 状态机逻辑异常 | 可能导致 UB 或 panic | 1 | `AtomicStatus` 封装阻止直接写入；`Status` 为 `#[repr(u8)]`，编译器验证判别值范围 |

## Panic 安全性

- `PanicGuard` 保证在 unwind 时将状态转为 `Failed`。
- `Failed` 状态是终态 — 所有后续访问均 panic。
- `try_call_once` 错误路径：在重置状态为 `Uninitialized` 之前显式
  `mem::forget`（`PanicGuard`），防止正常错误处理中的二次 panic。
- 即使 `mem::forget` 失败（不可能 — 它是安全的），守卫也会在 drop 时
  将单元格标记为 `Failed`，这是保守（安全）的结果。

## 故障管理

`ktypes` 通过以下机制处理故障：

- **错误传播**：`try_call_once` 返回 `Result<&T, E>`，调用者可处理错误。
- **中毒检测**：所有 `Failed` 状态的访问立即 panic，防止使用不一致状态。
- **无错误码**：模块不返回 POSIX 错误码，通过 Rust 的 `Result` / `panic`
  机制处理异常。

## 隐私分析

本模块为纯同步原语，不处理用户数据、不执行 I/O、不涉及网络通信。
不直接涉及用户隐私问题。需确保使用本模块的内核子系统正确管理各自的数据生命周期。

## 已知限制

1. **自旋等待无上限**：`wait()` 和 `poll()` 自旋无超时。如果初始化线程
   被无限期抢占，自旋线程浪费 CPU。在内核早期启动场景可接受，但不应在
   启动后的热路径中用于长时间初始化闭包。

2. **无优先级反转保护**：自旋线程无法提升初始化线程的优先级。

3. **中毒不可逆**：`Once` 一旦中毒（`Failed`），永远不可再用。
   `try_call_once` 出错时单元格会重置为 `Uninitialized`，可重试。
   但 panic 是不可逆的。

4. **`AtomicU8` lock-free 假设**：在 `AtomicU8` 非 lock-free 的目标上，
   正确性不受影响但性能下降。当前所有 x-kernel 支持的目标均具有
   lock-free `AtomicU8`。

## 审计清单

修改 `ktypes` 时需验证：

- [ ] 每个 `unsafe` 块均有 `SAFETY:` 注释说明不变量。
- [ ] 新增的 `u8 → Status` 转换不绕过 `AtomicStatus`。
- [ ] `try_call_once_slow` 中的状态转换符合 `design.md` 中的状态机图。
- [ ] 新增原子操作的内存序不低于 `design.md` 中表格的要求。
- [ ] `Drop` 实现正确处理所有新增状态。
- [ ] `PanicGuard` 覆盖新代码中的所有 panic 路径。
