# ktime - 设计文档

## 定位

`ktime` 是系统 timekeeping 语义层：基于 `khal::time` 提供的单调硬件时钟计算
realtime，并统一维护 realtime 关联（correlation）、累计睡眠时间、初始化和读取接口。
`khal` 只提供底层硬件时间能力，`drivers::rtc` 只提供 persistent-clock sample。

## 架构

```text
hardware counter -> khal::time (monotonic) --+
RTC sample (drivers::rtc, boot only) --------+--> ktime --> SystemTime
Confirmed sleep interval (future PM) -------+          --> BoottimeInstant
```

依赖方向：

```text
ktime (timekeeping) -> khal (硬件单调时钟)
                     -> ktime_types (语义值类型)
khal -> ktime_types
```

`ktime` 依赖 `khal`，`khal` 不依赖 `ktime`，因此 timekeeping 的演化（NTP
调频、多时钟源选择等）不需要改动硬件层。

## 范围

- `initialize_realtime`：用 persistent-clock sample 建立 realtime 关联，
  幂等，第一个 sample 生效。
- `set_realtime_checked`：运行时把墙钟重新关联到当前单调时刻
  （settimeofday 路径）。范围校验失败返回 `RealtimeOutOfRange` 错误
  （墙钟早于 `CLOCK_MONOTONIC` 或超过上界），由调用方映射为 `EINVAL`。
- `realtime_and_monotonic`：在同一关联读锁内采样一次单调时钟，返回对应的
  realtime/monotonic 值对；用于需要保留事件发生时刻两套时间的消费者。
- `realtime`：当前墙钟时间；未初始化时回退为 Unix epoch 加单调流逝时间
  和已注入的睡眠时间。
- `realtime_deadline_to_monotonic`：把 realtime 截止时间换算到单调时钟域。
- `boottime`：返回单调时钟加累计睡眠时间，使用独立的 `BoottimeInstant` 类型。
- `inject_sleeptime`：原子推进 realtime 和累计睡眠时间，不修改底层 monotonic。

## 设计决策

- realtime = realtime_base + (monotonic_now - monotonic_base)，速率 1:1。
- boottime = monotonic_now + sleep_time；初始 sleep_time 为零，墙钟设置和初始化
  不重置该累计值。
- `Timekeeper` 把关联和累计睡眠时间放在同一 `SpinRwNoIrq` 中：读路径（syscall/fs 时间戳）与写路径
  （启动初始化、settimeofday、睡眠补偿）互斥，且可在 IRQ 上下文安全读取。
- `set_realtime_checked` 不内置授权检查；调用者（syscall 层）负责
  Linux 特权校验。墙钟相对 `CLOCK_MONOTONIC` 的下界与上界由函数内置
  的范围校验保证，失败以 `RealtimeOutOfRange` 错误返回。
- 未初始化时回退 `SystemTime::UNIX_EPOCH + elapsed + injected sleep`，
  保证早期启动可用。
- 补偿在提交前校验累计 sleep_time、当前 boottime 及推进后的 realtime；
  realtime 使用现有 settimeofday 上界，任何错误均不修改状态。零增量为无操作。
- `SleepTimeError` 区分范围超限（`OutOfRange`）与单调基准无效
  （`InvalidMonotonicBaseline`），后者需要调用方处理计数器/基准一致性。
- 读取 boottime 对表示范围溢出采用饱和处理，避免未来底层流逝时间导致回绕。

## 调用约束 / 执行上下文

所有接口可在中断、早期启动和普通任务上下文使用；
内部锁为自旋锁，不阻塞、不睡眠。

## 睡眠补偿边界

算法对应 Linux `kernel/time/timekeeping.c` 中
`__timekeeping_inject_sleeptime()` 的核心时钟语义：REALTIME 和 BOOTTIME
增加 delta，MONOTONIC 不增加 delta。状态布局沿用本项目关联模型。

调用者必须先建立真实 suspend/resume 边界，保证传入的是尚未计入
MONOTONIC 的非负睡眠时间，并且每次睡眠仅结算一次。底层时钟的冻结、
恢复基准和回退修复属于 PM/架构工作，不能由此接口推断。

本阶段没有生产调用方，不改变 `CLOCK_BOOTTIME` syscall、`sysinfo.uptime`
或现有 timer 行为。后续 PM 集成必须在恢复依赖这些时钟的执行之前，完成
时间注入及 timer 通知/重臂；该接口不具备 Linux 外层
`timekeeping_inject_sleeptime64()` 的 timer 通知功能。

后续 BOOTTIME 消费者接入清单：

- `core/ksyscall/src/time/queries.rs`：`CLOCK_BOOTTIME` 读取。
- `core/ksyscall/src/sys.rs`：`sysinfo.uptime`。
- `process/ktimer/src/posix_timer.rs`：BOOTTIME 定时器及截止时间换算。
- `process/kfd_objects/src/timerfd.rs`：BOOTTIME timerfd。
- `fs/filesystems/devfs/src/nodes/event.rs`：BOOTTIME 事件时间戳。

这些消费者应统一使用 `ktime::boottime()`，定时器同时完成恢复通知与重臂，
并用非零睡眠补偿的集成用例验证，不能仅验证未注入时与 MONOTONIC 相等。

## 验证

单元测试使用独立 `Timekeeper` 和显式 monotonic 样本，不修改运行内核的
全局时钟。覆盖运行时间与睡眠时间的分离、重复及小数秒累计、墙钟设置对
BOOTTIME 的独立性、零增量、范围和累计溢出回滚、底层计数回退。
