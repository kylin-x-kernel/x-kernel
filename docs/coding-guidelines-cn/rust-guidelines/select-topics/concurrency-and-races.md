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

### 按执行上下文选择锁类型 (`lock-by-context`) {#lock-by-context}

应根据真实并发边界
选择约束最小的锁，
不要默认使用关中断自旋锁。

- 普通任务上下文中，
  若路径可能阻塞、等待、睡眠，
  或进入 poll/wake 相关逻辑，
  使用 `ksync::Mutex` 等睡眠锁。
- 在普通任务上下文中，
  若只是很短的非阻塞临界区，
  且 IRQ 处理路径不会访问同一状态，
  使用 `SpinNoPreempt`。
- 只有当本地 IRQ 处理程序
  或其他中断上下文代码
  会与当前路径竞争同一状态，
  或 API 必须可在中断上下文调用时，
  才使用 `SpinNoIrq`。
- 只有在调用者已经保证
  整个临界区内中断和抢占都已关闭时，
  才使用 `SpinRaw`，
  并明确记录这一前置条件。

如果不存在中断上下文竞争者，
就不要使用 `SpinNoIrq`。
它会扩大临界区语义，
让后续引入阻塞路径时更容易埋下 bug。

```rust
// 正确——任务上下文使用睡眠锁
fn update_connection(&self) {
    let mut conn = self.conn.lock(); // ksync::Mutex<_>
    conn.apply_update();
    conn.wait_queue.notify_all(true);
}

// 正确——任务上下文中的短小非阻塞自旋锁
fn push_local_stat(&self, delta: usize) {
    let mut stats = self.stats.lock(); // SpinNoPreempt<_>
    stats.rx_packets += 1;
    stats.rx_bytes += delta;
}

// 错误——没有 IRQ 竞争者却使用了关中断锁
fn update_state(&self) {
    let mut state = self.state.lock(); // SpinNoIrq<_>
    state.advance();
}
```

### 持有自旋锁时不得再获取睡眠锁 (`no-sleepable-lock-under-spinlock`) {#no-sleepable-lock-under-spinlock}

一旦持有自旋锁，
后续路径就必须保持非阻塞。
在释放自旋锁之前，
不要进入 `ksync::Mutex`、
`RwLock`、`Semaphore`、`WaitQueue`、
`block_on`、`sleep`
或类似路径。

这也包括框架提供的包装锁。
如果驱动或运行时回调
是在某个自旋锁保护下进入的，
应把该锁的作用域
限制在真正的寄存器或队列访问上，
释放后再去触碰可睡眠的子系统状态。

```rust
// 正确——先在自旋锁下取出设备事件，再释放锁
let event = self.device.with_mut(|dev| dev.poll_event())?;
match event {
    Some(event) => {
        let mut conn = self.conn.lock(); // 自旋锁作用域结束后再拿睡眠锁
        conn.handle(event);
    }
    None => {}
}

// 错误——在框架自旋锁作用域内进入睡眠锁
self.device.with_mut(|dev| {
    let mut conn = self.conn.lock();
    conn.handle(dev.poll_event()?);
    Ok::<_, Error>(())
})?;
```

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
