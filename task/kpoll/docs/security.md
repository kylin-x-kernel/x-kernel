# kpoll — 安全与可靠性分析

## 信任模型

```
用户态 syscall / 内核子系统
        │
        │ Pollable / PollContext / PollSet API
        v
┌──────────────────────────────────────────┐
│ kpoll                                    │
│  - PollRegistrations 生命周期由调用方持有 │
│  - PollSet 槽表由 SpinNoIrq 保护         │
│  - Completion done token 由 SpinNoIrq 保护│
│  - registration 仅持 Weak<source>        │
└──────────────────────────────────────────┘
        │
        │ Waker::wake（锁外）
        v
   调度器 / 任务恢复
```

- 调用方负责：持有 registration owner、register 后 recheck、把
  `PollRegisterError` 映射为用户可见错误。
- `kpoll` 负责：token 正确性、cancel/wake 线性化、wake 不持锁调用 Waker、
  `Waker::clone`/cancel-path drop 不在 `SpinNoIrq` 内执行、注册扩容失败可返回。
- `Completion` 负责：Linux-like done token 消费、`complete_all` sticky 完成态、
  poll waiter wake。它不负责阻塞当前 task，调用方必须先检查、注册后再检查。

## 外部边界 / 攻击面

经检查，`kpoll`：

- **不直接访问用户内存**；
- **不直接操作 MMIO / PIO / DMA**；
- **不解析 firmware / DT / ACPI**；
- **间接受用户控制的量**：并发 waiter 数量、poll fd 数量、epoll interest 数。

因此主要攻击/滥用面是：

- 大量并发 registration 造成内存压力；
- 忘记持有 `PollRegistrations` 导致挂死或 stale waiter；
- 在错误锁层级调用 `wake` 引发死锁；
- 把 wake 当成可靠状态通知而忽略 recheck。
- 把 `Completion::register` 当成完成状态检查而忽略 token recheck。
- 在旧 waiter 尚未完成观察时 `reinit()` 一个 `complete_all()` 后的 completion。

## unsafe 代码清单

当前 `kpoll` 实现本身不包含 `unsafe` 代码块。单元测试中的自定义
`RawWaker` 使用 `unsafe`，每个站点都有 `SAFETY:` 说明，且仅用于 unittest。

## 内存安全不变量

1. 每个 active slot 的 `(slot, id)` 唯一；注销必须同时匹配两者。
2. `next_id` 单调递增且不回绕复用；耗尽返回 `IdExhausted`。
3. `PollRegistration` 只持 `Weak<PollSetInner>`，不能延长 source 生命周期。
4. wake 摘取 slot 后，对应 token 永久失效；迟到 Drop 为 no-op。
5. 复合注册中途失败时，已成功项仍由 owner 持有，clear/drop 会完整回滚。
6. `Completion::complete()` 只在非 sticky 状态累积 ordinary token，且不会把
   ordinary token 计数推进到 `complete_all()` 的 sticky sentinel；`try_wait()` 消费
   一个 ordinary token，但不消费 sticky sentinel。
7. `Completion::register()` 不改变 `done`，不隐式阻塞，必须由调用方在注册后重新
   检查真实条件或 token。

## 线程安全

- `PollSet: Send + Sync`：内部 `SpinNoIrq` 保护槽表。
- `PollRegistration` / `PollRegistrations`：按所有权在单逻辑等待路径上移动；
  不要求跨线程共享同一个 owner。
- `Waker::wake` 可重入 `PollSet::wake`；因锁外 drain，不会在同一把锁上死锁。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 大量 waiter 耗尽内存 | 中 | 用户并发 poll/epoll 放大 | `try_reserve` 返回 `NoMemory`；syscall 映射 `ENOMEM` |
| T-02 | 等待结束后残留 waiter | 中 | 缺少 owner / 忘记 clear | API 强制 `PollContext`；RAII Drop 注销 |
| T-03 | ABA 误删新 waiter | 高 | slot 复用后旧 guard drop | token 含单调 `id` |
| T-04 | IRQ wake 持锁重入死锁 | 高 | 外层锁内调用 `wake` 且回调反取锁 | wake 锁外执行；`kirq::notify` 先 clone 再 unlock；register clone / cancel drop 均在锁外 |
| T-05 | OOM panic 代替错误返回 | 中 | 注册路径裸 `push`/`with_capacity` | 注册路径统一 `try_reserve`；已知限制仅限 `PollSet::new` 的 `Arc::new` |
| T-06 | 迟到 wake 被当成最终状态 | 低 | cancel 与 wake 竞态 | 文档约定 wake 可迟到；调用方必须 recheck |
| T-07 | Completion lost wake | 高 | 只 register 不 recheck，或把 wake 当完成态 | rustdoc 和 design 强制 `try_wait -> register -> try_wait` 协议 |
| T-08 | Completion `reinit()` 竞态 | 中 | `complete_all()` 后旧 waiter 未完成观察就重置 | 文档沿用 Linux 约束：调用方负责保证旧 waiter 已退出前一代完成态 |
| T-09 | `complete()` 惊群 | 低 | `PollSet` 当前 wake-all | 只有一个 waiter 消费一个 token；后续可扩展 wake-one 优化 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 注册失败 | 堆扩容失败 / ID 耗尽 | 本次等待立即失败 | 用户收到 `ENOMEM` | 3 | 错误传播到 syscall/async |
| F-02 | 丢失唤醒 | check/register 竞态 | 任务继续睡眠 | 阻塞 hang | 2 | register 后强制 recheck；block_on woke flag |
| F-03 | 误删并发等待 | 按 Waker 去重注销 | 另一等待永不醒 | hang | 1（已消除） | 每逻辑等待独立 token |
| F-04 | wake 路径分配失败 | 热路径新建 buffer | IRQ 分配/panic | 系统不稳 | 1（已消除） | 锁内只移出 slot table 并重置元数据；遍历和 `wake()` 都在锁外执行 |
| F-05 | source 先销毁 | fd/设备释放早于 waiter | 等待者无唤醒 | hang | 2 | source Drop 唤醒全部 waiter |
| F-06 | Completion token 被误判为 IRQ teardown 条件 | M3 只检查 `try_wait()`，未重查 `in_flight == 0` | teardown 提前返回或长期等待 | handler 生命周期错误 | 2 | M3 必须把 completion 作为 wake source，真实条件仍由 `in_flight` 决定 |

## 故障管理

- 注册失败：返回 `PollRegisterError`，由 `ktask`/`ksyscall` 映射为
  `KError::NoMemory` 或 `InvalidInput`（`InvalidState`）。
- 正常取消：`PollRegistrations` Drop/clear。
- 迟到 wake：允许，不视为错误。
- 不在正常控制流使用 panic；unittest 中的断言除外。

## 隐私分析

模块不处理用户数据内容，只保存任务 `Waker` 与 registration 元数据。

## 已知限制

1. `PollSet::new` 仍依赖 infallible `Arc::new`；仓库尚无通用 `Arc::try_new`。
2. `Waker::clone` 在极端自定义 waker 实现中仍可能分配；内核默认 `KWaker`
   为 Arc 引用计数。
3. 单次 `wake` 广播成本仍是 O(n)；lifecycle guard 防止无限累积。锁内成本只包括
   detach slot table 和重置元数据，实际遍历/唤醒在锁外执行。
4. IRQ 等多等待者事件源通过调用方提供的 `PollContext` 直接注册，
   registration 生命周期由跨越 `Pending` 的 `PollRegistrations` 管理。
5. `Completion::complete()` 当前基于 `PollSet::wake()` 广播唤醒，不提供 Linux
   `complete()` 的 FIFO wake-one 性能语义。
6. `Completion` 没有 timeout、interruptible 或 killable wait wrapper；这些属于
   `ktask`/future 层或后续里程碑。

## 审计清单

- [ ] 所有 `Pollable::register` / `register_poll` 都通过 `PollContext`。
- [x] 每个 `Poll::Pending` 路径都有活着的 `PollRegistrations`（含 signal /
  wait / interruptible / knet-rx；注册失败不得在无 waiter 时入睡）。
- [x] register 之后一定有 readiness recheck（含 `poll_interrupt` Ready 竞态）。
- [ ] 注册增长使用 `try_reserve`，wake 路径锁内不分配。
- [x] `wake` 不在持锁状态下调用 `Waker`（含 `kirq::notify` 全局表锁）。
- [x] `Completion` 文档要求 register 后 recheck，并说明 `complete_all/reinit` 约束。
- [ ] 外层锁顺序不会与 wake 回调反向。
- [ ] epoll interest 在 DEL/ONESHOT/Drop 时清空 registration，MOD 原地更新配置并重装 registration。
- [ ] 测试覆盖 >64 waiter、取消注销、token ABA、wake 重入。
