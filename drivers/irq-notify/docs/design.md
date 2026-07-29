# irq-notify — 设计文档

`irq-notify` 是 x-kernel 专属的 IRQ source 等待者通知层。它把
`device-res` 中断 handler 返回的 source bitmap 转换成 `PollSet` wakeup，
供普通任务上下文中的驱动或协议栈继续处理实际工作。

## 边界

- `device-res` 只描述资源和 `IrqEvent` 数据语义，不保存 waker，不依赖
  x-kernel 的 poll 模型。
- `kdriver` 在共享 IRQ line handler 中聚合各 handler 返回的 `IrqEvent`，再调用
  `irq_notify::dispatch_sources()` 分发 source bitmap。
- `knet` 等普通任务上下文代码通过 `irq_notify::register_source_waker()` 和
  `PollContext` 注册某条 IRQ 的某个 logical source。

## 数据结构

`IRQ_SOURCE_WAITERS` 是一个全局 `SpinNoIrq<Vec<IrqSourcePollSet>>`。
每个条目包含：

- IRQ number；
- logical source index；
- 对应的 `PollSet`。

注册时，若已有同一 `(irq, source)` 条目，则通过 `PollContext` 把当前逻辑等待
注册进现有 `PollSet`；否则创建新条目。调用方持有的 `PollRegistrations` 必须
跨越 `Pending`，注册失败会返回 `PollRegisterError`。

分发时，`dispatch_sources(irq, sources)` 在锁内把匹配的 `PollSet` clone 到
固定栈缓冲（大小为 `IRQ_EVENT_SOURCES`），释放全局表锁后再 `wake`。这样 IRQ
路径不会堆分配，也不会在持有全局表锁时执行 waker 回调。

## 并发语义

`register_source_waker()` 在任务上下文调用，用于阻塞前注册等待者；调用方必须
在每轮 poll 通过 `PollRegistrations::context()` 创建短生命周期 context。
`dispatch_sources()` 在 IRQ dispatch 路径调用：锁内只做短扫描与 `PollSet`
clone，锁外再 `wake`；不推进设备协议栈，不执行阻塞操作，也不在 IRQ 路径上
堆分配。
