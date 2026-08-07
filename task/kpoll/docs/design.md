# kpoll — 设计文档

## 定位

`kpoll` 是 x-kernel 中 I/O readiness 查询与 one-shot waiter 注册的核心原语。
`poll`/`select`/`epoll`、阻塞 read/write、信号等待、IRQ 桥接和 TIPC 等子系统
都通过它把“条件未就绪”转换为可被唤醒的异步等待。

## 背景

旧实现使用固定 64 槽数组保存 `Waker`，溢出时静默覆盖，导致大量共享 fd 上的
waiter（例如 hackbench）丢失唤醒并永久阻塞。随后的无界 `Vec<Waker>` 修复了
丢唤醒，但缺少：

- 随等待生命周期注销的 registration；
- 可失败的注册扩容；
- wake 路径零分配保证。

本重构引入 per-registration token + RAII owner，对齐 Linux `poll_wqueues`
“可扩容、可失败、随调用结束回收”的语义边界，同时保留 Rust async/`Waker`
模型。

## 范围

```
task/kpoll/
├── src/
│   ├── lib.rs           # Pollable trait 与公共导出
│   ├── events.rs        # IoEvents
│   ├── source.rs        # PollSet / 槽表 / wake
│   ├── completion.rs    # Completion token state + PollSet wake source
│   ├── registration.rs  # PollRegistration / PollRegistrations / PollContext
│   └── tests.rs
└── docs/
    ├── design.md
    └── security.md
```

主要消费方：

- `task/ktask`：`poll_io`、`interruptible`、task interrupt/join、GC wait；
- `fs/kvfs`：`FileOperations::register_poll`；
- `core/ksyscall`：poll/select、wait、signal；
- `process/kfd_objects`：eventfd/epoll 等；`fs/kvfs`：pipe/FIFO；
- `net/knet`、`io/ktty`、`arch/kirq`、`tee/tipc*`。

## 架构

```
Pollable::register(context, events)
        │
        v
   PollContext ──register──> PollSet
        │                       │
        │                       ├─ slot table (SmallVec inline + fallible spill)
        │                       └─ SpinNoIrq state
        v
 PollRegistrations (owner across Poll::Pending)
        │
        └─ Drop / clear() ──unregister──> 对应 slot
```

| 组件 | 职责 |
|------|------|
| `IoEvents` | Linux poll 事件位 |
| `PollSet` | IRQ-safe 广播源；持有 active waiter 槽表 |
| `PollRegistration` | 单个 registration 的 RAII guard（Weak 回指 source） |
| `PollRegistrations` | 一次逻辑等待拥有的全部 guards |
| `PollContext` | 当前 poll 轮次的短生命周期注册能力 |
| `Pollable` | 就绪快照 + 通过 `PollContext` 注册 |
| `Completion` | Linux-like completion token + poll waiter wake source |

## 调用约束 / 执行上下文

- **`PollSet::wake` 可在 IRQ 中调用**：内部使用 `SpinNoIrq`；锁内原地摘取 waker
  并 `clear` 槽表以保留 heap capacity，锁外调用 `Waker::wake`。整次 wake 只获取
  一次 spin lock，不再二次加锁回收 buffer。
- **`register` / `unregister` 可在任务上下文调用**；也可能与 IRQ wake 并发。
  `Waker::clone` 在加锁前完成；cancel 摘取的 `Waker` 在解锁后 drop，避免自定义
  waker 回调在 `SpinNoIrq` 内重入。
- **不得在持有可能与 scheduler/Waker 回调反向的外层锁时调用 `wake`**。
  `kirq::notify` 在唤醒前克隆 `PollSet` 并释放全局表锁。
- **注册可失败**：扩容失败返回 `PollRegisterError::NoMemory`；ID 耗尽返回
  `IdExhausted`；目标未就绪返回 `InvalidState`。上层映射为 `KError::NoMemory` /
  `ENOMEM` 或 `InvalidInput` / `EINVAL`。
- **调用方必须持有 `PollRegistrations` 跨 `Poll::Pending`**；timeout、signal、
  Ready 或 future drop 时自动注销。
- **wake 是提示而非状态转移**：允许迟到 wake；正确性依赖 register 后 recheck。
- **`PollSet::new` 内部 `Arc::new` 目前仍可能在 OOM 时走全局分配器 panic**；
  该限制不扩展到每次 register。
- **`Completion` 不阻塞当前 task**：它只提供 token 状态和 poll waiter 注册。
  blocking wait 必须由 `ktask`/future 层用 `try_wait -> register -> recheck`
  协议实现，低层 `kirq` 可以持有并 signal completion 而不依赖 scheduler。

## 状态机

```
register()
   │
   v
 Queued ──guard drop/cancel──> Cancelled (slot freed)
   │
   ├── wake()/source Drop ──> Detached ──锁外──> Waker::wake
   │
   └── 迟到 guard drop ──> No-op（token 已失效）
```

注销与 wake 由同一把 `SpinNoIrq` 线性化：

- cancel 先赢：不 wake；
- wake 先赢：允许一次迟到 wake。

## 算法流程

### 一次阻塞等待（`poll_io`）

1. 执行非阻塞操作 / readiness 检查。
2. `WouldBlock` 时用 `PollRegistrations::context(cx)` 清理上一轮 registration。
3. `pollable.register(&mut context, events)?`；失败立即返回 `NoMemory`。
4. 再次执行检查，封闭 check/register 竞态。
5. 仍阻塞则 `Pending`，owner 保留 registration。
6. 被唤醒、超时、中断或 drop 时，RAII 注销全部 source。

### `PollSet::wake`

1. 持锁检查 `active`；为 0 则返回。
2. 用 `mem::replace` 把整个 slot table 移出 `State`，并重置 `active/free_head`。
3. 释放锁。
4. 锁外遍历旧 slot table，逐个调用 occupied `waker.wake()`。

### `PollSet::register` / cancel

1. 锁外 `Waker::clone`。
2. 持锁安装 owned waker；`next_id` 仅在 slot 安装成功后递增。
3. cancel 持锁摘取 waker，解锁后再 drop。

### `Completion`

`Completion` 对齐 Linux `struct completion` 的 `done + waitqueue` 语义，但把
waitqueue 换成 `PollSet`：

1. 初始 `done == 0`，`try_wait()` 返回 `false`。
2. `complete()` 增加一个 token 并 wake 当前注册 waiter。ordinary token 数最多
   保持到 sticky sentinel 前一位，只有 `complete_all()` 会进入 sticky 完成态。
   由于 `PollSet` 是
   broadcast source，该 wake 当前会唤醒所有 waiter；只有一个 waiter 能消费这个
   token，其他 waiter 必须 recheck 后继续等待。
3. 多次 `complete()` 会累积 token。
4. `complete_all()` 把 `done` 置为 sticky sentinel，所有当前和后续 `try_wait()`
   都返回 `true`，直到调用 `reinit()`。
5. `complete_all_defer_wake()` 只把 `done` 置为 sticky sentinel 并返回内部
   `PollSet` clone，让调用方能先释放自己的外层锁，再执行 wake。
6. `reinit()` 把 `done` 重新置为 `0`。和 Linux `reinit_completion()` 一样，
   调用方必须保证旧 waiter 已经完成对前一代完成态的观察。

`Completion::register(context)` 只注册 wake source，不检查或消费 token。正确调用
协议必须是：

```
if completion.try_wait() {
    return Ready;
}
completion.register(context)?;
if completion.try_wait() {
    return Ready;
}
return Pending;
```

这个二次检查封闭 complete-before-register 和 register-before-complete 的 lost wake
窗口。

## 并发模型

- 共享状态：`SpinNoIrq<State>`。
- 注册不去重：同一 task 的多个逻辑等待拥有独立 token，避免一个 guard 误删另一个。
- 复杂度：注册/注销 O(1)（除扩容）；唤醒 O(n)；不再做锁内 `will_wake` 扫描。
- epoll 在 `EpollInterest` 上长期持有 `PollRegistrations`（`Mutex`），并用 generation
  过滤已被 drain、正在锁外执行的旧 `InterestWaker`；file `register_poll` 在锁外执行。
  替换/清空 owner 时用 `mem::replace` / `mem::take` 把旧值移出后再 drop，避免在
  `Mutex` 内嵌套 source `SpinNoIrq` 注销。

## 设计决策

| 决策 | 理由 | 放弃方案 |
|------|------|----------|
| RAII registration + token | 用所有权表达生命周期，自动回收 | 共享 `Vec<Waker>` / 按 Waker 注销 |
| `SmallVec` inline + `try_reserve` | 常见路径零堆，溢出可失败 | 固定上限数组；裸 `Vec::push` |
| wake 路径移出 slot table 后锁外唤醒 | IRQ 热路径单次加锁、锁内不分配、不调用 waker | 锁内构造临时 waker buffer；二次加锁 recycle capacity |
| `PollContext` 强制入口 | 禁止把裸 Waker 永久塞进 fd | 保留旧 `register(&Waker)` 兼容 API |
| registration 持 `Weak` | 防引用环，允许 source 先销毁 | registration 持强引用 |
| `Completion` 放在 `kpoll` | `kirq` 后续可持有 wait source，且不依赖 `ktask`/`ksync` | 放入 `ktask` 或 `ksync` 后形成 crate 依赖环 |
| `Completion` 不提供 blocking wait | 保持 scheduler-agnostic；阻塞由 `ktask`/future 层组合 | 在低层 poll crate 调用 scheduler |

## Drop / 资源释放

- `PollRegistration::Drop`：若 token 仍匹配 occupied slot，则摘链回收。
- `PollRegistrations::Drop` / `clear`：逐个 drop guard。
- `PollSetInner::Drop`：摘取全部 waiter 并锁外唤醒，避免析构后永久阻塞。
- `Completion`：drop 时通过内部 `PollSet` drop 唤醒 waiter；调用方仍必须通过
  readiness recheck 判断完成状态是否仍存在。
- epoll interest Drop / DEL / ONESHOT：通过清空其 `PollRegistrations` 注销；MOD
  原地更新同一个 interest 的配置并重新安装 registration。
