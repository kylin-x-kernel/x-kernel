# ktime - 安全与可靠性分析

## 信任模型

`ktime` 信任 `khal` 提供单调、稳定的硬件时钟读数，信任调用者在 syscall 边界
完成特权校验。`set_realtime_checked` 内置墙钟范围校验（见外部边界），
syscall 层不再负责 CLOCK_MONOTONIC 约束。RTC 驱动只提供 sample，不直接修改
timekeeper 状态。

## 外部边界 / 攻击面

该 crate 不访问用户内存、设备、网络或文件系统。外部输入通过
`initialize_realtime`/`set_realtime_checked` 传入的 `SystemTime` sample 进入；
`SystemTime` 构造本身规范化并校验分量范围。`set_realtime_checked` 对
sample 执行内置范围校验：墙钟不得早于 `CLOCK_MONOTONIC` 当前值，也不得
超过上界 `MAX_SETTABLE_UNIX_SECONDS`（Linux `KTIME_SEC_MAX`）；校验失败
返回 `RealtimeOutOfRange` 错误，不应用任何部分修改。

## 内存安全不变量

无 `unsafe` 代码（`#![deny(unsafe_code)]`）。状态为普通 Rust 值，由
`SpinRwNoIrq` 保证访问互斥。

## 线程安全

`REALTIME_CORRELATION` 由读写锁保护，可在多 CPU 与 IRQ 上下文安全访问；
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

## 审计清单

- 新增写路径是否经过特权与范围校验。
- 锁守卫是否保持 IRQ 安全（不允许在持锁时睡眠）。
- realtime 与 monotonic 换算是否始终使用同一快照。
