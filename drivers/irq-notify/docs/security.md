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
2. **IRQ 路径轻量化**：`dispatch_sources()` 只扫描 waiter 表、把匹配的
   `PollSet` clone 到固定栈缓冲并在锁外唤醒，不执行设备 IO、协议栈推进、
   阻塞操作或堆分配。
3. **source 取值边界**：source index 必须小于 `IRQ_EVENT_SOURCES`，超出范围的
   source 不参与唤醒。
4. **等待者所有权**：注册必须进入调用方的 `PollRegistrations`；timeout、
   cancellation 或 future drop 会通过 owner 注销仍在队列中的等待者。

## 威胁与缓解

| 编号 | 威胁 | 影响 | 缓解 |
|------|------|------|------|
| T-01 | source index 越界导致非法位移或误唤醒 | 错误唤醒或未定义行为风险 | 分发时检查 `source < IRQ_EVENT_SOURCES` |
| T-02 | 不同 IRQ 上相同 source index 互相唤醒 | 无关设备 poll task 被唤醒 | 分发时同时匹配 IRQ number 和 source bit |
| T-03 | IRQ 路径执行重型工作或堆分配 | 中断延迟、锁竞争、死锁或 OOM | 固定栈缓冲 clone + 锁外 wake；协议推进留在任务上下文 |
| T-04 | 等待 future 结束后遗留 waker | 失效任务被迟到唤醒 | `PollContext` 把 registration 绑定到调用方 owner |
| T-05 | 注册失败后仍 `Pending` 且无 waiter | 任务永久失唤醒 | 调用方必须处理 `PollRegisterError`（重试/返回错误） |

## 限制

waiter 表使用线性扫描，适合当前少量设备 IRQ source 场景。若后续设备数量或
source 数量显著增加，需要改成按 `(irq, source)` 分桶的结构。
