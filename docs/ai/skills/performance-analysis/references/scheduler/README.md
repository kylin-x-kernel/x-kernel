# 调度性能（scheduler）

X-Kernel 调度相关性能分析的入口。挂在
`docs/ai/skills/performance-analysis/` 下，与 [lock-stat](../lock-stat.md)
同级。

面向 **guest 调度基准**（schbench / hackbench）和 **EEVDF 唤醒/交接**，
不是通用 lock 争用或 boot/panic 分诊。

## 何时读这里

- schbench / hackbench 的 wakeup / request 延迟或 RPS 异常；
- 改调度后 p50 与 p99.9 **反向**变化；
- 怀疑 place/pick、唤醒选核、IRQ 延迟抢占、futex 交接、affinity 继承。

不要从这里开始查：

- 锁争用 → [../lock-stat.md](../lock-stat.md)
- 启动/panic/hang → `docs/ai/skills/problem-diagnosis/SKILL.md`

## 子文档

| 文档 | 内容 |
|------|------|
| [wakeup-latency.md](wakeup-latency.md) | schbench 测法、基线、p50 vs p99.9、调查流程、`/proc/sched_stat` |
| [eevdf-wake.md](eevdf-wake.md) | EEVDF place/pick、buddy、WF_SYNC、调度侧禁区 |
| [infra.md](infra.md) | IRQ 尾抢占、affinity/PID1、`block_on` 竞态 |

建议阅读顺序：先 [wakeup-latency.md](wakeup-latency.md) 分类症状，再按需进
[eevdf-wake.md](eevdf-wake.md) 或 [infra.md](infra.md)。

## 相关代码

- `task/ksched/src/eevdf.rs`
- `task/ktask/src/run_queue.rs`
- `task/ktask/src/future/mod.rs`
- `arch/khal/src/irq/manager.rs`
- `process/kfutex/src/table.rs`
- `core/ksyscall/src/task/{clone,sched}.rs`
- `posix/process/src/init_process.rs`
