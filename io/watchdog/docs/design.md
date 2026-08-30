# io/watchdog — 软/硬锁检测看门狗设计文档

## 定位

`io/watchdog` 提供 X-Kernel 的软锁（softlockup）与硬锁（hardlockup）检测：

- 软锁：每 CPU watchdog 任务周期性更新时间戳，定时器回调检查时间戳是否过期；
- 硬锁：依赖 NMI（`nmi` feature）周期检查本 CPU 定时器中断计数是否仍在前进；检测失败后触发全局 rendezvous、收集快照并 panic。

依赖：`khal`（NMI / percpu / 时间）、`ktask`（任务、定时器回调、快照）、`ktime_types`。

## 背景

中断被长时间屏蔽或任务长时间不调度时，系统可能处于不可响应状态。软锁检测依赖调度与定时器本身（可能受影响）；硬锁检测依赖独立于普通 IRQ 的 NMI 源，能在普通中断完全停摆时仍然触发。X-Kernel 以 PMU 周期计数器作为 NMI 源（经 `kplat::NmiPeriodic`），本 crate 是 NMI 的唯一消费方。

## 范围

- `src/init.rs`：`init_primary` / `init_secondary` / `init_common` / `init_nmi_watchdog` / `init_softlockup_detection`；
- `src/lockup_detection.rs`：每 CPU `LockupDetection` 状态、阈值、`WatchdogTask` 实现；
- `src/watchdog_task.rs`：`WatchdogTask` trait、每 CPU 任务队列、互斥死锁检查；
- `src/rendezvous.rs`：失败触发的全局 rendezvous 状态机；
- `src/lib.rs`：导出。

## 架构

```text
                  定时器中断                     NMI（PMU 周期源）
                     │                               │
        ┌────────────┴───────────┐          ┌────────┴────────┐
        ▼                        ▼          ▼                 ▼
  touch/check_softlockup     timer_tick   check_watchdog_tasks
  （watchdog 任务 + 定时器） （hardlockup 计数） （NMI 上下文）
        │                        │                │
        └────────────────────────┴────────────────┤
                        检测失败                    │
                             ▼                    ▼
                      软锁：log + dump        rendezvous（全局）
                                              cause CPU 收集全部
                                              CPU 快照 → panic
```

## 调用约束 / 执行上下文

| 入口 | 执行上下文 | 约束 |
|------|------------|------|
| `init_primary` / `init_secondary` | 主核 / 从核启动 | 每 CPU 一次 |
| `init_nmi_watchdog` | 启动期（NMI feature） | 检查 `khal::nmi::mode()`，`None` 时记录并禁用 hardlockup |
| NMI 回调（`enable_periodic_nmi` 的 handler） | NMI 上下文 | 只允许 NMI 安全操作：原子、快照、kprint；禁止普通 IRQ 自旋锁 |
| 定时器回调（`timer_tick` / 软锁检查） | 定时器中断，IRQ 关闭 | 每 CPU 原子访问 |
| watchdog 任务 | 每 CPU 固定核任务 | 只写软锁时间戳、睡眠 4s |

## 状态机

### hardlockup 检测（每 CPU）

`hrtimer_interrupts`（定时器中断递增）与 `hrtimer_interrupts_saved`（上次 NMI 检查值）比较：计数未前进且已初始化 → hardlockup 条件成立（`check_hardlockup` 返回 `true`；`WatchdogTask::check` 对其取反，健康任务返回 `true`）。

### 全局 rendezvous

```text
Idle ──try_trigger（首个失败 CPU 原子迁移）──▶ Triggered
Triggered ──各 CPU NMI 中 mark_arrived──▶（等待 all_arrived_mask）
Triggered ──cause CPU 收集/打印快照后 mark_dump_done──▶ DumpDone
DumpDone ──cause CPU panic（系统停止）──▶ 终态
```

`reset()` 提供返回 Idle 的路径（注释明确：其他 CPU 可能仍在 NMI 自旋，使用需谨慎）。

## 算法流程

### NMI hardlockup 检查（每次 NMI）

1. `check_watchdog_tasks()`：遍历本 CPU 队列（`HardLockupDetection`、`MutexDeadlock`），失败则返回任务名；
2. 有失败：`ktask::snapshot::nmi_begin()` 成功则 `rv::try_trigger()`（CAS Idle→Triggered，成功者成为 cause CPU）；
3. 所有 CPU 在各自 NMI 中 `mark_arrived` 并 `nmi_collect_local()`；
4. cause CPU `wait_all_arrived_strong()` 后打印失败信息、`nmi_dump_all`、`mark_dump_done`、panic；其余 CPU 自旋等待 `is_dump_done`。

### 软锁检测

- watchdog 任务（每 CPU，固定核）每 4s `touch_softlockup`；
- 定时器回调每 tick `timer_tick()` + 检查 `now - soft_timestamp > 20s`，超限则 log + `dump_sched_stats` + `dump_cpu_tasks`，并按阈值限速。

## 并发模型

- `LockupDetection` 为每 CPU 状态，字段为原子量（`AtomicU64` / `AtomicU32` / `AtomicBool`），NMI 与定时器中断并发访问安全；
- `WATCHDOG_TASK_QUEUE` 为每 CPU `Vec`，注册仅在 per-CPU init（迁移不可能），NMI 中只读遍历；
- rendezvous 用全局原子（`PHASE` / `CAUSE_CPU` / `ARRIVED_BITMAP`）+ 自旋，NMI 上下文中不取锁；
- NMI 路径绝不触碰普通 IRQ 自旋锁（避免 pseudo-NMI 抢占持锁路径造成同 CPU 自死锁）。

## 设计决策

- **阈值**：软锁 20s、硬锁 10s；watchdog 任务 4s 触碰一次，20s 阈值内 5 次机会，避免误报；
- **强 rendezvous（无超时）**：cause CPU 必须等所有 CPU 到达才 dump，避免漏 CPU 快照；代价是任一 CPU 无法进入 NMI 时会永久等待（此时系统已不可用，可接受）；
- **NMI 机制不可用时显式降级**：启动日志提示 hardlockup 关闭，不静默失败；
- **快照重入保护**：`nmi_begin()` 失败（已有快照在跑）时跳过本次 dump；
- **软锁报告限速**：每阈值周期最多报告一次。

## Drop / 资源释放

每 CPU 状态与内核同生命周期，无运行期释放；`reset()` 仅在需要重启 rendezvous 时使用。
