# ktime - 设计文档

## 定位

`ktime` 是系统 timekeeping 语义层：基于 `khal::time` 提供的单调硬件时钟计算
realtime，并统一维护 realtime 关联（correlation）、累计睡眠时间、初始化和读取接口。
`khal` 只提供底层硬件时间能力，`drivers::rtc` 只提供 persistent-clock sample。

## 架构

```text
hardware counter -> khal::time (monotonic) --+
RTC sample (drivers::rtc, boot only) --------+--> ktime --> SystemTime
Confirmed sleep interval (future PM) -------+          --> BoottimeInstant
```

依赖方向：

```text
ktime (timekeeping) -> khal (硬件单调时钟)
                     -> ktime_types (语义值类型)
khal -> ktime_types
```

`ktime` 依赖 `khal`，`khal` 不依赖 `ktime`，因此 timekeeping 的演化（NTP
调频、多时钟源选择等）不需要改动硬件层。

## 范围

- `initialize_realtime`：用 persistent-clock sample 建立 realtime 关联，
  幂等，第一个 sample 生效。
- `set_realtime_checked`：运行时把墙钟重新关联到当前单调时刻
  （settimeofday 路径）。范围校验失败返回 `RealtimeOutOfRange` 错误
  （墙钟早于 `CLOCK_MONOTONIC` 或超过上界），由调用方映射为 `EINVAL`。
- `realtime_and_monotonic`：在同一关联读锁内采样一次单调时钟，返回对应的
  realtime/monotonic 值对；用于需要保留事件发生时刻两套时间的消费者。
- `realtime`：当前墙钟时间；未初始化时回退为 Unix epoch 加单调流逝时间
  和已注入的睡眠时间。
- `realtime_deadline_to_monotonic`：把 realtime 截止时间换算到单调时钟域。
- `boottime`：返回单调时钟加累计睡眠时间，使用独立的 `BoottimeInstant` 类型。
- `inject_sleeptime`：原子推进 realtime 和累计睡眠时间，不修改底层 monotonic。

## 设计决策

- realtime = realtime_base + (monotonic_now - monotonic_base)，速率 1:1。
- boottime = monotonic_now + sleep_time；初始 sleep_time 为零，墙钟设置和初始化
  不重置该累计值。
- `Timekeeper` 把关联和累计睡眠时间放在同一 `SpinRwNoIrq` 中：读路径（syscall/fs 时间戳）与写路径
  （启动初始化、settimeofday、睡眠补偿）互斥，且可在 IRQ 上下文安全读取。
- `set_realtime_checked` 不内置授权检查；调用者（syscall 层）负责
  Linux 特权校验。墙钟相对 `CLOCK_MONOTONIC` 的下界与上界由函数内置
  的范围校验保证，失败以 `RealtimeOutOfRange` 错误返回。
- 未初始化时回退 `SystemTime::UNIX_EPOCH + elapsed + injected sleep`，
  保证早期启动可用。
- 补偿在提交前校验累计 sleep_time、当前 boottime 及推进后的 realtime；
  realtime 使用现有 settimeofday 上界，任何错误均不修改状态。零增量为无操作。
- `SleepTimeError` 区分范围超限（`OutOfRange`）与单调基准无效
  （`InvalidMonotonicBaseline`），后者需要调用方处理计数器/基准一致性。
- 读取 boottime 对表示范围溢出采用饱和处理，避免未来底层流逝时间导致回绕。

## 时钟变化通知

`change.rs` 持有独立 REALTIME / BOOTTIME 的 `kpoll::PollEvent`。
墙钟设置和首次初始化仅发布 REALTIME；非零睡眠注入同时发布两个域。
失败更新及零注入不发布。状态与事件 generation 在 TIMEKEEPER 写锁内提交，
先释放该锁，再唤醒，waker 不得在时间锁内执行。

订阅者在任务上下文预先初始化通知源，先读取 generation，再读取时间谓词，
注册后重新检查 generation / 原时钟截止时间。`ClockChangeListener` 将此规则
封装为一次性 Future；Drop 注销。通知仅提示重算，不代表定时器已经到期。
一次性等待只公开 `ClockChangeListener` 与构造函数；generation 采样留在模块内部，
不暴露无消费者的底层注册 API。timerfd 运行时使用独立持久观察者。

通知源首次创建可能分配内存；只有订阅 API 触发创建，写路径只读取已存在的
源，不在 IRQ 中首次分配。已注册 waker 必须支持 IRQ 安全唤醒。
两个 domain 的 generation 均按 usize 回绕，假定单个等待不会跨越 usize::MAX 次更新。

## 调用约束 / 执行上下文

读时、初始化、设置及睡眠注入使用 IRQ 安全自旋锁；注册/等待 API 只用于任务
上下文。唤醒成本随等待者数量增长，调用者不得持有被这些 waker 重入的外层锁。
此设计复用 kpoll 的注册与清理能力，没有新增 CI stage。

## 睡眠补偿边界

通用层仅接收经过确认、尚未计入 MONOTONIC 的睡眠时间；调用者负责正式
suspend/resume 事务、底层时钟冻结及基准恢复、来源选择和恰好一次结算。
本 PR 不接入 RTC 观测、ARM64 fixup、PM 生产调用方或宿主协议。

BOOTTIME / 绝对 REALTIME 等待与定时器保留原始域截止时间，转换到单调时钟
只是当前硬件定时器的编程快照；时钟变化后必须重算。
MONOTONIC 与相对 REALTIME 不订阅时钟变化。ITIMER_REAL 沿用 MONOTONIC。
`CLOCK_BOOTTIME` 和 `sysinfo.uptime` 读取通用 BOOTTIME。
事件输入历史时间戳的 BOOTTIME 接入留待独立改动，本 PR 不改变其数据布局。

## 验证

时间算术使用独立 Timekeeper；通知测试使用独立 Changes / PollEvent，
不注入运行内核的全局睡眠时间。消费者测试覆盖原域检查、周期累计和相对/
绝对 REALTIME 交替武装。UT 不替代正式 PM 或真实宿主休眠测试。

## 持久运行时观察者

`observer.rs` 在一次性 poll 监听之外提供持久运行时订阅。
`ClockReading` 及其更新序号在 TIMEKEEPER 锁内采样；回调在解锁后使用该次
提交的快照执行。注册表保存弱引用，通过写时复制保持已取得的通知快照不变。
通知在表锁内克隆 Arc，锁外调用观察者；通知本身不分配内存。
释放注册凭据会停用订阅，与观察者 Arc 是否仍存活无关；已取得的回调仍可能完成。
替换注册表时清理失效弱引用槽位，存储可能保留此前的并发高水位。
回调和观察者最终析构必须兼容 IRQ 上下文，通知遍历成本为 O(N)。

只有表 Arc 唯一且 Vec 有剩余容量时，注册才在 IRQ 锁内原地追加且不分配。
否则在锁外清理失效槽位并准备几何扩容的新表，再在锁内校验并替换。
持有旧 Arc 会禁止原地修改，因此指针校验能检测替换和注册竞争。
无并发快照时，连续注册的表维护成本为摊还 O(1)。128 个订阅的回归约束
替换次数，并检查通知及注销。已有通知快照时，注册仍需 O(N) 复制，
暂时保留两张表；并发注册可能重试。该回归不是延迟上界或扩展性基准，
持续重叠的通知快照仍可能导致反复复制。

替换表根据清理后的存活项数量预留容量，不使用旧槽位数量。
反复创建/关闭会回收失效槽位，容量不会随历史注册次数持续增长。
独立的 4096 次注册/释放回归检查表长度和容量有界，并确认已关闭订阅
不再收到通知；它不是堆内存或延迟基准。替换前仍可能保留此前的并发高水位。

通知和观察者回归分别放在 `ut-tests/change.rs`、`ut-tests/observer.rs`，
由对应生产模块通过 `unittest::path_mod_test!` 挂载，保留私有接口测试边界。

## 暂停与恢复结算事务

`src/suspend.rs` 提供 `begin_suspend_accounting`、`resume_suspend_accounting` 和
`cancel_suspend_accounting`。同一 TIMEKEEPER 锁保护时钟、事务状态与提交。开始时
保存一致 ClockReading，并分配本次启动内不复用的私有标识；同一时刻只允许一轮事务。
恢复验证凭证及单调基准，在副本上调用不含事务策略的共享注入算术，成功后一次提交
时钟偏移和事务结束；旧式注入入口独立检查活动事务，不再靠先清状态绕过策略。
失败保持原状态，可重试或取消；零注入结束事务但不发通知。非零提交解锁后复用
REALTIME/BOOTTIME 通知及观察者；取消不改时钟，不发送通知。

这是 Linux timekeeping_suspend/resume 职责中结算部分的适配，不是完整 PM 实现。
期间读时钟与定时器仍运行，墙钟设置保留；恢复在当前关联上增加睡眠时长。
调用方先恢复连续且不包含待补偿睡眠的 MONOTONIC，再提交确认的睡眠时长。
本层不推断 RTC 差值，不冻结时钟，不恢复 clocksource/clockevent，不停止设备或 CPU，
也不提供宿主协议。旧 inject_sleeptime 非零调用在事务期间被拒绝；零调用仍是无操作。
凭证只保证同一轮提交一次，不能识别调用方在后续新事务或旧注入 API 中重复报告同一睡眠。

凭证拥有绑定到签发 Timekeeper 的内部回收守卫，Drop 自动取消仍属于本凭证的事务，
不补偿时间、不通知、不恢复硬件；已完成或已取消的旧凭证析构不会清除新事务。
错误路径的普通提前返回也能释放结算所有权。Drop 短暂取得时间写锁，不得在持有
该锁或 NMI 中执行；PM owner 仍负责硬件及任务的回滚，不得 mem::forget 凭证。
主动绕过析构不保证恢复所有权。标识耗尽时失败，不回绕。没有生产 PM 调用者，
也不以单元测试证明
真实硬件暂停、多核恢复顺序或跨宿主时钟可信性。
