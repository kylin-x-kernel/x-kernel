# kfd_objects - 安全与可靠性分析

## 概述

`kfd_objects` 负责 fd-backed kernel object 的内部状态和运行时回调。
当前对象是 `TimerFd`、`EventFd`、`Signalfd` 和 `Epoll`。
主要风险来自：

- timer callback 与 `read/poll/settime` 的并发交互；
- fd close/drop 时的底层 timer handle 清理；
- `read()` / `poll()` / `gettime()` 的状态一致性；
- 非阻塞路径的可见性语义；
- `epoll` interest table 与 ready queue 的一致性。

本 crate 当前不包含手写 `unsafe` 代码。

## 信任模型

```text
userspace syscall args
   │
   v
ksyscall adapter
   │ validates ABI flags and syscall-specific user input
   v
kfd_objects::{Epoll, EventFd, Signalfd, TimerFd}
   │ owns object state, callbacks, read/write/poll semantics
   v
ktask timer runtime / generic fd readiness
```

- `ksyscall` 负责用户指针、flag、clockid 的 ABI 校验。
- `ksyscall` 或其它上层调用者负责选择创建文件时的 `Arc<Cred>`；本 crate 不假定存在
  当前用户线程。
- `kfd_objects` 信任 `ktask` timer runtime 的 handle/register/cancel 语义。
- `kfd_objects` 信任 `kfd`/`kresources` 在 fd 生命周期上维持 `VfsFile`
  及其 private data 的强引用。

## 状态不变量

- `read()` 只有在存在未消费 expiration 时才成功返回 8 字节计数。
- `poll(IN)` 与“是否有未消费 expiration”保持一致。
- `settime(disarm)` 后不再保留旧的 pending handle。
- `drop` 必须取消底层 timer handle，避免悬挂回调。
- `gettime()` 返回对象视角的 interval/remaining，而不是 syscall 临时状态。
- `EventFd::read/write/poll` 必须围绕同一计数器上限与 semaphore 语义保持一致。
- `Signalfd::read/poll` 必须围绕同一 pending signal 可见性与 mask 语义保持一致。
- `Epoll` 的 interest table、ready queue 与 trigger mode 必须围绕同一就绪语义保持一致。
- 匿名文件构造必须使用调用者显式提供的 credential，并只由 `VfsFile` 保存该快照。

## 并发模型

`TimerFd` 使用：

- `SpinNoIrq<TimerFdInner>` 保护核心 timer 状态；
- `SpinNoIrq<Option<TimerHandle>>` 保护底层 handle；
- nonblocking 标志由当前 `VfsFile` 保存；
- `PollSet` 用于读就绪唤醒。

timer runtime 回调与 `read/poll/settime` 可能并发发生。
因此所有对 `deadline/interval/expirations` 的更新都必须在同一把 `inner` 锁内完成。

`EventFd` 使用原子计数和两个 `PollSet`。
它没有外部 runtime 回调，但 `read/write/poll` 之间仍需对计数上限和就绪语义保持一致。

`Signalfd` 使用：

- `RwLock<SignalSet>` 保护当前 mask；
- nonblocking 标志由当前 `VfsFile` 保存；
- `PollSet` 维护可读唤醒。

它不拥有 signal 队列本身；队列 owner 仍在当前线程的 signal state。
因此 `read()` / `poll()` 必须始终通过当前线程 signal manager 观察 pending signal。

`Epoll` 使用：

- `SpinNoPreempt<HashMap<...>>` 保护 interest table；
- `SpinNoPreempt<VecDeque<...>>` 保护 ready queue；
- `Mutex<()>` ctl lock 串行化 `ADD` / `MOD` / `DEL` 的 table 更新、
  registration 重装和失败回滚；
- 每个 `EpollInterest` 内的 `SpinNoPreempt` config 保护 event mask、user data
  和 trigger mode 的原地更新；
- `AtomicBool` 跟踪 interest 是否已经入队；
- `PollSet` 维护 `epoll_wait` 侧唤醒。

watched file 的 fd table 解析现在由 syscall adapter 完成。
因此 backend 只需围绕 interest 键值稳定性、ready queue 去重和
watched file 失效后的清理维护同一个状态机。
`MOD` 保持 interest identity 稳定，只重置配置和 registration；失败时在同一个
ctl lock 临界区内恢复旧配置，避免 ready queue 或旧 source wake 引用到被替换的
旧对象，也避免并发 `MOD` rollback 覆盖另一个成功更新。

## 主要风险

| 编号 | 风险 | 影响 | 缓解 |
|------|------|------|------|
| T-01 | timer 到期与 `read()` 并发，导致 expiration 丢失或重复消费 | 中 | 统一在 `inner` 锁内 tick/consume |
| T-02 | `settime()` 重编程时旧 handle 未取消 | 中 | 先 `cancel_pending_timer()` 再更新状态 |
| T-03 | `drop` 后底层仍保留回调 | 高 | `Drop` 中取消 handle |
| T-04 | `poll()` 与 `read()` 对 readiness 观察不一致 | 中 | 两者都先 `tick(clock_now(...))` 再判断 |
| T-05 | `eventfd` 计数溢出或 `poll(OUT)` 与写入条件不一致 | 中 | `fetch_update` 与 `poll()` 共享同一上限判断 |
| T-06 | `eventfd` semaphore/普通模式读路径分叉导致计数错误 | 中 | 两种语义都经同一原子更新路径处理 |
| T-10 | `signalfd` mask 更新与 `read/poll` 观察不一致 | 中 | mask 经 `RwLock` 更新，并在更新后唤醒 poller 重新观察 |
| T-11 | `signalfd` 将不可捕获信号暴露给用户态 fd 语义 | 低 | syscall adapter 统一移除 `SIGKILL` / `SIGSTOP` |
| T-12 | `epoll` ready queue 去重失效导致重复唤醒、事件风暴或 `MOD` 后返回旧事件配置 | 中 | `in_ready_queue` 位与 ready queue 统一维护；`MOD` 原地更新稳定 interest identity 并重装 registration |
| T-13 | `epoll` oneshot / edge-triggered 状态机错误 | 高 | `TriggerMode` 集中建模并在消费路径统一更新 |
| T-14 | watched file 失效后 interest 清理不完整 | 中 | ready 消费路径在 `Weak` 升级失败后立即回收 interest |
| T-15 | 内核任务创建匿名 fd 时因隐式读取当前用户凭据而 panic | 高 | 构造函数调用 `current_cred()`，但当前 task 不是 `Thread` | 构造函数接收显式 `Arc<Cred>`；syscall adapter 传当前快照，内核调用者选择初始或其它明确凭据 |

## 审计清单

- [ ] 新增对象仍然符合“fd-backed object owner”边界，而不是 syscall adapter。
- [ ] 新增状态更新是否与回调路径共用同一锁和不变量。
- [ ] `drop` 是否清理底层 runtime 绑定。
- [ ] `read/poll` 是否对外保持一致的 readiness 语义。
- [ ] `EventFd` 计数上限、poll 可写性和 semaphore 语义是否同步更新。
- [ ] `Signalfd` 的 mask 更新、pending signal 观察和 `poll(IN)` 语义是否保持一致。
- [ ] `Epoll` 的 interest table、ready queue、oneshot/edge 触发语义是否保持一致。
- [ ] 新增文件构造路径是否显式接收凭据，且没有把 credential 重复保存到对象状态。
