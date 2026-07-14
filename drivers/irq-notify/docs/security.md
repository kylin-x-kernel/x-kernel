# irq-notify — 安全与可靠性分析

`irq-notify` 只处理内核内部 IRQ source 到 waker 的通知，不接收用户输入，
不访问设备寄存器，也不持久化数据。

## 信任边界

- 信任调用者传入的 IRQ number 来自已注册的设备 IRQ。
- 信任 `device-res::IrqEvent` 的 source bitmap 由对应设备 handler 正确生成。
- 信任 `PollSet` 在 wake 路径中不会阻塞。

## Invariant

1. **按 IRQ 隔离**：分发时必须同时匹配 IRQ number 和 source bit，避免不同
   IRQ 上相同 source index 互相唤醒。
2. **IRQ 路径轻量化**：`dispatch_sources()` 只扫描 waiter 表并唤醒 `PollSet`，
   不执行设备 IO、协议栈推进或阻塞操作。
3. **source 取值边界**：source index 必须小于 `IRQ_EVENT_SOURCES`，超出范围的
   source 不参与唤醒。

## 威胁与缓解

| 编号 | 威胁 | 影响 | 缓解 |
|------|------|------|------|
| T-01 | source index 越界导致非法位移或误唤醒 | 错误唤醒或未定义行为风险 | 分发时检查 `source < IRQ_EVENT_SOURCES` |
| T-02 | 不同 IRQ 上相同 source index 互相唤醒 | 无关设备 poll task 被唤醒 | 分发时同时匹配 IRQ number 和 source bit |
| T-03 | IRQ 路径执行重型工作 | 中断延迟、锁竞争或死锁 | 只唤醒 `PollSet`，实际协议推进留在任务上下文 |

## 限制

waiter 表使用线性扫描，适合当前少量设备 IRQ source 场景。若后续设备数量或
source 数量显著增加，需要改成按 `(irq, source)` 分桶的结构。
