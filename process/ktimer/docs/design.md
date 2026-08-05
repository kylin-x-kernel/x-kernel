# ktimer - 设计文档

## 定位

`ktimer` 管理进程共享的 `setitimer` 和 POSIX timer 状态，并把到期事件转换为
`TimerDelivery`。系统调用解析和信号实际派发位于 crate 外部。

## 范围

- `interval_timer.rs`：间隔、deadline、重装和遗漏周期计算。
- `posix_timer.rs`：POSIX clock 选择、绝对/相对 deadline 和 overrun 状态。
- `manager.rs`：每进程 timer 集合与轮询入口。
- `runtime.rs`：基于 `MonotonicInstant` 的全局 alarm 队列。

## 架构

```text
syscall ABI -> TimeSpan/SystemTime/Instant
                    |
           ProcessTimerManager
             |              |
          ITimer         PosixTimer
             \              /
              MonotonicInstant alarm queue -> TimerDelivery
```

`TimerInstant` 是 crate 内部闭集，区分 realtime、monotonic、boottime 和 process CPU
时钟。`ITimer` 保存 `TimeSpan` interval 和 `Option<TimerInstant>` deadline，不保存带单位整数。

## 调用约束 / 执行上下文

timer manager 由所属 `ProcessRuntime` 的 mutex 保护。alarm task 可睡眠，使用
`timeout_at` 等待最早的 `MonotonicInstant`。CPU timer 轮询依赖调用者提供已采样的
`TimeSpan` 用户态和内核态 CPU 时间。

## 状态机

disarmed timer 没有 deadline。设置非零 value 后进入 armed；到期时一次性 timer
回到 disarmed，周期 timer 从旧 deadline 前进整数个 interval，避免漂移。
不可表示的非零 deadline 在修改 timer 状态前返回错误，不能被当作 disarm 请求。

## 设计决策

- 不同 clock 的 deadline 使用 enum，而不是共享纳秒标量。
- realtime alarm 在入队时根据当前 `SystemTime` 和 `MonotonicInstant` 计算等待时长，
  alarm 队列本身只处理 monotonic deadline。
- `snapshot` 接收带时钟域的当前时刻，只返回 interval 和 remaining `TimeSpan`。
