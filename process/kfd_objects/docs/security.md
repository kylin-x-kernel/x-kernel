# kfd_objects - 安全与可靠性分析

## 概述

`kfd_objects` 负责 fd-backed kernel object 的内部状态和运行时回调。
当前对象是 `TimerFd`、`EventFd` 和 `PipeObject`。
主要风险来自：

- timer callback 与 `read/poll/settime` 的并发交互；
- fd close/drop 时的底层 timer handle 清理；
- `read()` / `poll()` / `gettime()` 的状态一致性；
- 非阻塞路径的可见性语义。

本 crate 当前不包含手写 `unsafe` 代码。

## 信任模型

```text
userspace syscall args
   │
   v
ksyscall adapter
   │ validates ABI flags and syscall-specific user input
   v
kfd_objects::{EventFd, PipeObject, TimerFd}
   │ owns object state, callbacks, read/write/poll semantics
   v
ktask timer runtime / generic fd readiness
```

- `ksyscall` 负责用户指针、flag、clockid 的 ABI 校验。
- `kfd_objects` 信任 `ktask` timer runtime 的 handle/register/cancel 语义。
- `kfd_objects` 信任 `kfd`/`kresources` 在 fd 生命周期上维持对象强引用。

## 状态不变量

- `read()` 只有在存在未消费 expiration 时才成功返回 8 字节计数。
- `poll(IN)` 与“是否有未消费 expiration”保持一致。
- `settime(disarm)` 后不再保留旧的 pending handle。
- `drop` 必须取消底层 timer handle，避免悬挂回调。
- `gettime()` 返回对象视角的 interval/remaining，而不是 syscall 临时状态。
- `EventFd::read/write/poll` 必须围绕同一计数器上限与 semaphore 语义保持一致。
- `PipeObject::read/write/poll` 必须围绕同一 buffer、reader/writer 计数和
  `PIPE_BUF` 原子写入语义保持一致。

## 并发模型

`TimerFd` 使用：

- `SpinNoIrq<TimerFdInner>` 保护核心 timer 状态；
- `SpinNoIrq<Option<TimerHandle>>` 保护底层 handle；
- `AtomicBool` 保存 nonblocking 标志；
- `PollSet` 用于读就绪唤醒。

timer runtime 回调与 `read/poll/settime` 可能并发发生。
因此所有对 `deadline/interval/expirations` 的更新都必须在同一把 `inner` 锁内完成。

`EventFd` 使用原子计数和两个 `PollSet`。
它没有外部 runtime 回调，但 `read/write/poll` 之间仍需对计数上限和就绪语义保持一致。

`PipeObject` 使用：

- `Mutex<PipeState>` 保护 ring buffer 与 reader/writer 计数；
- 两个 `PollSet` 分别维护读端和写端唤醒；
- 每个 endpoint 自己的 `AtomicBool` 保存 nonblocking 标志。

`PipeReadEnd` / `PipeWriteEnd` 的 `drop`、`read/write`、`poll` 可能并发发生。
因此 reader/writer 计数、EOF/HUP/ERR 语义、以及 resize 过程中 buffer 迁移，
都必须通过同一个 `PipeState` 锁维护。

## 主要风险

| 编号 | 风险 | 影响 | 缓解 |
|------|------|------|------|
| T-01 | timer 到期与 `read()` 并发，导致 expiration 丢失或重复消费 | 中 | 统一在 `inner` 锁内 tick/consume |
| T-02 | `settime()` 重编程时旧 handle 未取消 | 中 | 先 `cancel_pending_timer()` 再更新状态 |
| T-03 | `drop` 后底层仍保留回调 | 高 | `Drop` 中取消 handle |
| T-04 | `poll()` 与 `read()` 对 readiness 观察不一致 | 中 | 两者都先 `tick(clock_now(...))` 再判断 |
| T-05 | `eventfd` 计数溢出或 `poll(OUT)` 与写入条件不一致 | 中 | `fetch_update` 与 `poll()` 共享同一上限判断 |
| T-06 | `eventfd` semaphore/普通模式读路径分叉导致计数错误 | 中 | 两种语义都经同一原子更新路径处理 |
| T-07 | pipe reader/writer 计数失配导致 EOF/HUP/ERR 错误 | 中 | `drop`、`poll`、`read/write` 统一经 `PipeState` 锁维护计数 |
| T-08 | pipe resize 丢数据或发布未初始化字节 | 高 | resize 时先检查 occupied_len，再复制已读写切片并保持容量上限检查 |
| T-09 | pipe 写端在无 reader 时未正确抛出 `SIGPIPE` / `BrokenPipe` | 中 | 在写路径同一锁内检查 `readers == 0`，并统一走 signal + error 分支 |

## 审计清单

- [ ] 新增对象仍然符合“fd-backed object owner”边界，而不是 syscall adapter。
- [ ] 新增状态更新是否与回调路径共用同一锁和不变量。
- [ ] `drop` 是否清理底层 runtime 绑定。
- [ ] `read/poll` 是否对外保持一致的 readiness 语义。
- [ ] `EventFd` 计数上限、poll 可写性和 semaphore 语义是否同步更新。
- [ ] `PipeObject` 的 EOF/HUP/ERR、`PIPE_BUF` 原子写入和 resize 上限语义是否同步更新。
