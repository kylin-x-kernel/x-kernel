# 调度外围：IRQ、affinity、`block_on`

支撑唤醒延迟的外围路径。测法见 [wakeup-latency.md](wakeup-latency.md)；
EEVDF 算法见 [eevdf-wake.md](eevdf-wake.md)。

## IRQ 尾抢占被 exception 吃掉

- 现象：IPI / 本地 wake 已打 `need_resched`，却等到下一个 tick 才切。
- 机制：IRQ handler 里 drop `NoPreempt` 时，EL1 active-exception 标记仍在，
  ktask 拒绝抢占；exception 退出后没有第二次检查。
- 修法：设备 IRQ 完成后，短暂 **suspend** active-exception 标记，让
  enable-preempt 钩子在 IRQ 尾安全窗口切任务，再 resume
  （`arch/khal/src/irq/manager.rs`）。

## Affinity 继承 + PID 1 单核 pin

- 现象：`get_nprocs() == 1`，worker 全堆一核，延迟/RPS 离谱。
- 机制：Linux 式 `clone` 继承创建者 affinity；PID 1 若 `one_shot` pin 到
  boot CPU，后续用户进程全部继承单核 mask。
- 修法：
  - `spawn_init_process` 保持默认 **全部 online CPU** affinity；首跑在哪核无所谓；
  - `clone` 仍继承 creator mask（schbench 依赖此语义 pin worker）。

`sched_get/setaffinity` 应按 tid 解析；`setaffinity` 须先做 same-owner /
`CAP_SYS_NICE`（当前用 root）校验，再 `set_task_affinity`：ready 换队、
running 经 IPI/`preempt_resched` 迁出；迁不走则 `EBUSY`，禁止静默成功。

## `block_on`：wake-before-block 不要 yield

- 现象：future 已 ready，仍排在无关 runnable 后面。
- 机制：`Poll::Pending` 与提交 block 之间 waker 已触发，再 `yield_now()`
  会把 CPU 让给别人。
- 修法：清 wake 标志后 **立刻再 poll**（`task/ktask/src/future/mod.rs`）。
