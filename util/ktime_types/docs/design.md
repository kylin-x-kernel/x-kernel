# ktime - 设计文档

## 定位

`ktime` 提供内核内部统一的语义时间类型。它不读取时钟，也不编程硬件。
`khal::time` 提供单调硬件时钟，`ktime`（timekeeping 层）负责 monotonic 与
realtime 的关联。

## 范围

- `src/span.rs`：`TimeSpan` 非负时间长度。
- `src/instant.rs`：由 marker 类型区分的 `Instant<C>` 时钟域时刻。
- `src/system_time.rs`：带符号的 Unix 墙钟时间。
- `src/units.rs`：秒、毫秒、微秒和纳秒之间的固定单位换算常量。
- `src/lib.rs`：稳定的 crate 根公开重导出，不承载具体类型实现。

## 架构

```text
hardware counter -> khal::time ----+-> MonotonicInstant
RTC sample ------------------------+-> SystemTime
                                      |
                                      +-> TimeSpan arithmetic
```

`MonotonicInstant`、`BoottimeInstant`、`ProcessCpuInstant` 和
`ThreadCpuInstant` 使用不同 marker，禁止跨时钟域比较或相减。`core::time::Duration`
只通过 `TimeSpan::from_core` 和 `TimeSpan::into_core` 显式转换。

## 调用约束 / 执行上下文

所有类型都是纯值类型，不分配、不阻塞，可用于中断、早期启动和普通任务上下文。
从整数、ABI 或硬件表示构造时间值应只发生在相应边界。
反向转换同样只允许发生在硬件寄存器、ABI、序列化或第三方接口边界；
`as_nanos_u64_saturating` 明确表达无法表示时钳位到 `u64::MAX` 的策略。

## 设计决策

- `TimeSpan` 不作为 `Duration` 的别名，避免依赖代码无意混用两套 duration。
- `Instant<C>` 的泛型时钟域在编译期阻止 realtime、monotonic 和 CPU time 混用。
- `SystemTime` 保存规范化的秒和纳秒，允许表达 Unix epoch 之前的时间。
- `SystemTime::{MIN, MAX}` 统一定义墙钟类型的可表示边界。
- `SystemTime` 的 checked 加减直接处理秒和纳秒分量，避免高频墙钟路径经过
  `i128` 总纳秒的乘除规范化。
- checked 和 saturating 运算供外部输入及非严格排序路径使用；运算符只用于已建立顺序和范围不变量的内部路径。
