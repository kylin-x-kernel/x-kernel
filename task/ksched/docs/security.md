# ksched — 安全与可靠性分析

## 信任模型

```
ktask RunQueue（唯一调用方）
   │
   │ BaseScheduler safe API
   │
   v
┌──────────────────────────────────┐
│  FIFO / RR / CFS / EEVDF         │
└──────────────────────────────────┘
```

- 调用方必须保证：同一 RQ 上序列化访问；运行任务离开必经 `leave_current`。
- 调度器信任传入的 `SchedItem` 与任务状态已由 `ktask` 校验。

## 外部边界 / 攻击面

`ksched` 不直接接触用户内存、设备或固件输入。攻击面主要是：

- 错误的调度 API 使用顺序（漏 `leave_current`、重复入队）；
- 优先级等参数范围（nice 边界由各实现校验）。

## unsafe 代码清单

各算法实现主要依赖安全集合/`Arc`。FIFO/RR 的 intrusive list `remove` 含
`unsafe`，不变量由“任务只通过本调度器入链”维护；调用路径限于
`remove_task`。

## 内存安全不变量

1. Ready 任务的强引用仅由 ready 队列持有（外加调用方临时 `Arc`）。
2. EEVDF `curr` 不得持有 `Arc`；仅保存调度数值快照。
3. `pick_next_task` 前 `curr` 必须为空（已 `leave_current`）。
4. Exit 不得设置 PLACE_LAG；Block/Migrate 必须设置，供后续 `enqueue_task`。
5. 运行实体调度字段变更后，`curr` 快照必须同步刷新。

## 线程安全

`BaseScheduler` 本身不是可并发类型；并发安全由 `ktask` 的 per-CPU RQ 锁与
IRQ/preempt 约束提供。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 漏 `leave_current` 导致陈旧 `curr`/统计失真 | 高 | 新 leave 路径未接入统一 API | 统一 `leave_current`；EEVDF `pick_next` 断言 |
| T-02 | 调度器持有运行任务 `Arc` 拖住生命周期 | 高 | `curr` 强引用未释放 | `curr` 改为非 owning 快照 |
| T-03 | Exit 误设 PLACE_LAG | 中 | Exit 与 Block 共用 sleep 记账 | `CurrentDisposition::Exit` 单独分支 |
| T-04 | 迁移后源 RQ 仍计入任务权重 | 中 | Migrate 未清 current 记账 | Migrate 走 `leave_current` 清快照 |
| T-05 | `curr` 快照过期导致错误抢占/放置 | 中 | 改运行实体字段后未刷新 | `update_current`/`set_priority` 刷新；测试辅助显式同步 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | pick 时 curr 非空 | 调用顺序错误 | 断言失败 | 调度停止（暴露 bug） | 2 | 强制 leave 契约 |
| F-02 | 唤醒未 PLACE_LAG | Block/Migrate 未记 lag | 放置偏差 | 延迟/公平性劣化 | 3 | disposition 区分 + 单测 |
| F-03 | Block 后无额外强引用 | 调用方未 pin 任务 | `switch_to` 断言失败 | 调度停止 | 2 | `blocked_resched` 要求 `strong_count > 1` |

## 故障管理

- 契约破坏优先 `assert!`（编程错误，不应静默恢复）。
- 优先级越界返回 `false`，由上层决定是否向用户返回错误。

## 隐私分析

无用户数据；仅操作内核任务调度元数据。

## 已知限制

- CFS 在 `ktask` feature 接线仍指向历史 `axsched` 名称时不可用；算法本体在本 crate。

## 审计清单

- [ ] 新增离开路径是否只调用 `leave_current`。
- [ ] EEVDF `curr` 是否仍保持非 owning。
- [ ] Exit 是否避免 PLACE_LAG。
- [ ] 迁移后源 RQ 的 V/weight 是否不再包含迁出任务。
- [ ] Block 路径调用方是否另持强引用。
- [ ] 单测是否覆盖五种 disposition 与 pick-without-leave。
