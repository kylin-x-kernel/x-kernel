# ktimer - 安全与可靠性分析

## 信任模型

系统调用层负责把用户 ABI timespec 转成已验证的 `TimeSpan`。`ktimer` 信任传入的
process CPU time 是同一进程的当前累计值。

## 外部边界 / 攻击面

用户可选择 clock、绝对/相对模式、interval、deadline 和通知方式。clock ID 与 ABI
时间格式在 syscall/manager 边界验证；信号值保持为显式 ABI bit pattern。

## 内存安全不变量

该 crate 无裸指针硬件访问。`k_sigval` union 的读取仅保存用户提供的原始位，相关
`unsafe` 块由 ABI union 布局保证。

## 线程安全

每进程状态由外部 manager mutex 串行化。全局 alarm heap 由 `Mutex` 保护；新最早
deadline 入队后通过 event 唤醒 alarm task。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | realtime 与 monotonic deadline 混用 | 中 | 共享无类型标量 | `TimerInstant` 变体匹配，域不匹配立即暴露内部错误 |
| T-02 | timer deadline 或周期前进发生算术溢出 | 中 | 极大 value、interval 或逾期次数 | 设置时拒绝不可表示的 deadline 且保留原状态；周期前进使用 saturating/checked 运算并钳位 expiration count |
| T-03 | 旧 alarm entry 产生重复通知 | 低 | timer 重设后旧 entry 仍在 heap | poll 时重新读取 timer 状态，signal sequence 过滤陈旧通知 |

## 已知限制

boottime 当前与 monotonic 使用相同底层时钟读数，尚未单独累计 suspend 时间。

## 审计清单

- 新 clock 是否增加独立 `TimerInstant` 变体和转换规则。
- ABI 整数是否在进入 manager 前转换成语义类型。
- alarm queue 是否始终只接收 `MonotonicInstant`。
