# ktime - 安全与可靠性分析

## 信任模型

`ktime` 信任 `khal` 提供单调、稳定的硬件时钟读数，信任调用者在 syscall 边界
完成特权校验。睡眠补偿的调用方还必须确认睡眠边界、处理底层
MONOTONIC 基准并保证单次结算。`set_realtime_checked` 内置墙钟范围校验（见外部边界），
syscall 层不再负责 CLOCK_MONOTONIC 约束。RTC 驱动只提供 sample，不直接修改
timekeeper 状态。

## 外部边界 / 攻击面

该 crate 不访问用户内存、设备、网络或文件系统。外部输入通过
`initialize_realtime`/`set_realtime_checked` 传入的 `SystemTime` sample，
以及 `inject_sleeptime` 传入的 `TimeSpan` 进入；
`SystemTime` 构造本身规范化并校验分量范围。`set_realtime_checked` 对
sample 执行内置范围校验：墙钟不得早于 `CLOCK_MONOTONIC` 当前值，也不得
超过上界 `MAX_SETTABLE_UNIX_SECONDS`（Linux `KTIME_SEC_MAX`）；校验失败
返回 `RealtimeOutOfRange` 错误，不应用任何部分修改。

`TimeSpan` 从类型上排除负增量；注入前校验累计睡眠值和 BOOTTIME 的
表示范围，以及 REALTIME 加法和既有墙钟上界，失败返回
`SleepTimeError::OutOfRange` 且无部分修改。单调读数早于关联基准时返回
`SleepTimeError::InvalidMonotonicBaseline`，调用方可区分范围错误和底层基准异常。该接口不直接暴露给用户态，
不负责鉴别 RTC 样本、检测真实休眠或去重。

## 内存安全不变量

无 `unsafe` 代码（`#![deny(unsafe_code)]`）。状态为普通 Rust 值，由
`SpinRwNoIrq` 保证访问互斥。

## 线程安全

`TIMEKEEPER` 的关联及累计睡眠时间由同一读写锁保护，可在多 CPU 与 IRQ 上下文安全访问；
`REALTIME_INITIALIZED` 原子标记保证初始化只生效一次。

`realtime_and_monotonic` 返回自有值而非锁守卫。采样与关联换算都在同一
读临界区，后续墙钟更新不能改变已经缓存的值对。该只读接口不添加授权
或用户内存边界，也不写全局时间。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 非特权进程修改墙钟 | 高 | 直接调用 `set_realtime_checked` | syscall 层检查 privileged credential；`set_realtime_checked` 内置范围校验（墙钟不得早于 `CLOCK_MONOTONIC` 或超过 `MAX_SETTABLE_UNIX_SECONDS`），失败返回 `RealtimeOutOfRange`，syscall 层将其映射为 `EINVAL` |
| T-02 | 初始化输入超范围 | 中 | 越界 SystemTime sample | `SystemTime` 构造规范化分量；读取路径 checked/saturating 兜底 |
| T-03 | IRQ 上下文死锁 | 高 | 中断读取 realtime 时写者持锁 | 读写均使用 `SpinRwNoIrq`（关中断），写临界区极短 |
| T-04 | 墙钟回拨导致负 duration | 中 | realtime 被设置到过去 | `duration_since` 返回错误而非下溢，deadline 换算按已过期处理 |
| T-05 | 重复注入或注入已计入 MONOTONIC 的时间 | 高 | PM 调用方违反结算契约 | 调用方维护休眠事务和来源选择；此阶段未接入生产路径 |
| T-06 | 补偿溢出导致部分状态更新 | 高 | 过大 delta 或累计值 | 所有计算成功后才提交两个状态；失败保持原值 |
| T-07 | 时间更新后仍使用旧 deadline | 高 | 未来调用方未处理 timer 恢复 | 恢复流程须在依赖时钟的执行恢复前通知并重臂；本阶段不接入 timer |

## 审计清单

- 新增写路径是否经过特权与范围校验。
- 锁守卫是否保持 IRQ 安全（不允许在持锁时睡眠）。
- realtime 与 monotonic 换算是否始终使用同一快照。
- 睡眠补偿来源是否可信、是否已从 MONOTONIC 排除，是否只结算一次。
- timer 通知与恢复执行的顺序是否满足调用契约。
