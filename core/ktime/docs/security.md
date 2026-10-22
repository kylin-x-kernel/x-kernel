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
| T-07 | 时间更新后仍使用旧 deadline | 高 | 未来调用方未处理 timer 恢复 | 状态提交时发布 generation，解锁后唤醒；消费者保留原域截止时间并重新判断 |

## 审计清单

- 新增写路径是否经过特权与范围校验。
- 锁守卫是否保持 IRQ 安全（不允许在持锁时睡眠）。
- realtime 与 monotonic 换算是否始终使用同一快照。
- 睡眠补偿来源是否可信、是否已从 MONOTONIC 排除，是否只结算一次。
- timer 通知与恢复执行的顺序是否满足调用契约。

## 时钟通知的并发与资源契约

- 通知源只在任务订阅路径初始化；时间写路径不得触发首次分配。
- TIMEKEEPER 锁内仅发布 generation 和克隆唤醒源，锁外调用 waker。
- 等待注册通过 PollRegistrations 管理，取消时清理；分配失败返回错误。
- 注册前后的 generation 检查阻止遗漏已经发布的变化；唤醒后仍检查时间谓词。
- 注入来源和恰好一次结算仍由未来 PM 调用方保证，通知不能证明真实休眠。

## 持久观察者约束

观察者回调可以重叠，也可以与提交顺序不同。消费者必须使用提交快照，
拒绝早于当前武装的更新。`ClockChangeId` 使用半范围序号比较，
单次存活武装不得跨越 2^63 次更新。
回调期间不持有时间锁或注册表锁。观察者回调及最终析构不得睡眠或分配；
timerfd 通过预先准备的工作捕获保留运行时，捕获资源延迟至任务上下文回收。

IRQ 锁内插入要求 Arc 唯一且 Vec 有剩余容量；该路径不扩容、不清理槽位。
复制、几何扩容、清理和释放旧表均在该锁外执行。
已取得的通知快照会迫使注册使用写时复制，快照在注册期间保持不变。
独立回归检查新注册不会改变已取得的表，但不替代真实并发 IRQ 验证。

## 暂停结算凭证与恢复边界

事务标识、当前轮次及时间偏移由同一时间锁串行保护。凭证字段私有且不提供
构造或整数转换。重复、已取消或旧轮凭证不得提交；标识不回绕复用。恢复在副本
上验证所有算术后才提交，失败不能消耗有效凭证或修改任一时钟。零提交不通知。
非零旧式注入在事务活动期间被拒绝，避免绕过该轮结算；墙钟设置仍允许且不被恢复覆盖。
所有通知在时间锁释放后运行，沿用现有 IRQ 回调契约。无锁跨越暂停，无首次分配。

凭证只提供内核内结算所有权，不验证实际休眠、RTC 稳定性或宿主事件真实性。
调用方负责互斥 PM 流程、恢复 MONOTONIC 基准及排除睡眠时间；本层仅拒绝早于
开始快照或当前墙钟关联基准的恢复样本，无法识别看似正常的向前计数跳变。
凭证 Drop 只在签发 Timekeeper 的活动标识仍匹配时自动取消，不触发时间补偿或通知，
不会影响后续事务。析构取得时间写锁，不得在持有该锁或 NMI 中执行；不分配或调用回调。
正常错误返回可恢复结算所有权；显式 mem::forget 绕过析构不受支持，PM owner 必须
另行撤销自己的硬件准备。
独立 Timekeeper 的单测不修改全局时间，也不替代真实 PM 端到端验证。
