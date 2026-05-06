# 并发与竞态

并发代码的审查极其严格。
锁顺序、原子操作正确性、内存顺序
以及竞态条件分析都被明确要求。

### 建立并强制执行一致的锁顺序 (`lock-ordering`) {#lock-ordering}

从不同代码路径
以不同顺序获取两个锁
会导致潜在的死锁。
必须建立分层锁顺序并进行文档记录。

```rust
pub(super) fn set_control(
    self: Arc<Self>,
    process: &Process,
) -> Result<()> {
    // 锁顺序：进程组 -> 会话内部 -> 作业控制
    let process_group_mut = process.process_group.lock();
    // ...
}
```

另见：
PR [#2942](https://github.com/asterinas/asterinas/pull/2942)。

### 持有自旋锁时绝不执行 I/O 或阻塞操作 (`no-io-under-spinlock`) {#no-io-under-spinlock}

在执行 I/O 或阻塞操作时持有自旋锁是一个死锁风险。
应使用睡眠锁或重构代码以先释放锁。

```rust
// 正确——在 I/O 前释放自旋锁
let data = {
    let guard = self.state.lock(); // state: SpinLock<...>
    guard.pending_data.clone()
};
self.device.write(&data)?;

// 错误——持有自旋锁时执行 I/O
let guard = self.state.lock(); // state: SpinLock<...>
self.device.write(&guard.pending_data)?;
```

另见：
PR [#925](https://github.com/asterinas/asterinas/pull/925)。

### 谨慎使用原子操作 (`careful-atomics`) {#careful-atomics}

当多个原子字段
必须协同更新时，应使用锁。
仅当单个值
真正独立时才使用原子操作。

```rust
// 正确——锁保护相关字段
struct Stats {
    inner: SpinLock<StatsInner>,
}
struct StatsInner {
    total_bytes: u64,
    total_packets: u64,
}

// 错误——两个原子变量必须保持一致
// 但可能被观察到处于不一致状态
struct Stats {
    total_bytes: AtomicU64,
    total_packets: AtomicU64,
}
```

### 关键段不得跨越锁边界拆分 (`atomic-critical-sections`) {#atomic-critical-sections}

必须原子执行的操作
（检查 + 条件动作）
必须在同一次锁获取下完成。
将比较操作移出临界区
是一个正确性错误。

```rust
// 正确——检查与动作在同一锁下执行
let mut inner = self.inner.lock();
if inner.state == State::Ready {
    inner.state = State::Running;
    inner.start();
}

// 错误——TOCTOU 竞态：状态可能在
// 检查与动作之间发生变化
let is_ready = self.inner.lock().state == State::Ready;
if is_ready {
    self.inner.lock().state = State::Running;
    self.inner.lock().start();
}
```

另见：
PR [#2277](https://github.com/asterinas/asterinas/pull/2277)。
