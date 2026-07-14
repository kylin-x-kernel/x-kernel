# irq-notify — 设计文档

`irq-notify` 是 x-kernel 专属的 IRQ source 等待者通知层。它把
`device-res` 中断 handler 返回的 source bitmap 转换成 `PollSet` wakeup，
供普通任务上下文中的驱动或协议栈继续处理实际工作。

## 边界

- `device-res` 只描述资源和 `IrqEvent` 数据语义，不保存 waker，不依赖
  x-kernel 的 poll 模型。
- `kdriver` 在共享 IRQ line handler 中聚合各 handler 返回的 `IrqEvent`，再调用
  `irq_notify::dispatch_sources()` 分发 source bitmap。
- `knet` 等普通任务上下文代码通过 `irq_notify::register_source_waker()` 注册
  某条 IRQ 的某个 logical source waker。

## 数据结构

`IRQ_SOURCE_WAITERS` 是一个全局 `SpinNoIrq<Vec<IrqSourcePollSet>>`。
每个条目包含：

- IRQ number；
- logical source index；
- 对应的 `PollSet`。

注册时，若已有同一 `(irq, source)` 条目，则把 waker 注册进现有 `PollSet`；
否则创建新条目。

分发时，`dispatch_sources(irq, sources)` 遍历 waiter 表，只唤醒 IRQ number
匹配且 source bit 被置位的条目。

## 并发语义

`register_source_waker()` 在任务上下文调用，用于阻塞前注册等待者。
`dispatch_sources()` 在 IRQ dispatch 路径调用，只执行短临界区扫描和
`PollSet::wake()`，不推进设备协议栈，不执行阻塞操作。
