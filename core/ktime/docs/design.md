# ktime - 设计文档

## 定位

`ktime` 是系统 timekeeping 语义层：基于 `khal::time` 提供的单调硬件时钟计算
realtime，并统一维护 realtime 关联（correlation）、初始化和读取接口。
`khal` 只提供底层硬件时间能力，`drivers::rtc` 只提供 persistent-clock sample。

## 架构

```text
hardware counter -> khal::time (monotonic) --+
RTC sample (drivers::rtc) -------------------+--> ktime (correlation) --> SystemTime
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
- `set_realtime`：运行时把墙钟重新关联到当前单调时刻（settimeofday 路径）。
- `realtime`：当前墙钟时间；未初始化时回退为 Unix epoch 加单调流逝时间。
- `realtime_deadline_to_monotonic`：把 realtime 截止时间换算到单调时钟域。

## 设计决策

- realtime = realtime_base + (monotonic_now - monotonic_base)，速率 1:1。
- 关联状态由 `SpinRwNoIrq` 保护：读路径（syscall/fs 时间戳）与写路径
  （启动初始化、settimeofday）互斥，且可在 IRQ 上下文安全读取。
- `set_realtime` 不内置授权检查；调用者（syscall 层）负责 Linux 规则
  （墙钟不得移到 CLOCK_MONOTONIC 之前）与特权校验。
- 未初始化时回退 `SystemTime::UNIX_EPOCH + elapsed`，保证早期启动可用。

## 调用约束 / 执行上下文

所有接口均为纯函数式调用，可在中断、早期启动和普通任务上下文使用；
内部锁为自旋锁，不阻塞、不睡眠。
