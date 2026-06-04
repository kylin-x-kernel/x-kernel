# kthread — 安全与可靠性分析

## 信任模型

```text
syscall / posix / procfs / kservices / ktty / tee
       │
       │ safe API: current thread, process state, registry, signal, timer
       ▼
┌──────────────────────────────────────────┐
│ kthread                                  │
│                                          │
│  task extension: Box<Thread>             │
│  shared ProcessState                     │
│  weak task/process registry              │
│  signal and timer delivery bridge        │
│  pidfd process capability object         │
│  futex table selection                   │
│                                          │
│  unsafe boundary: TaskExt downcast       │
└──────────────────────────────────────────┘
       │
       ▼
ktask / kprocess / memspace / kresources / ksignal / ktimer / kfutex
```

- 调用者负责在 syscall 边界完成用户指针、权限、PID 可见性和参数合法性检查。
- `kthread` 负责维护 task extension 类型、当前用户线程访问、进程共享运行态和 registry lookup 不变量。
- `kthread` 不直接读写用户内存，不接收设备 DMA，不解析网络包。
- `current_process_state`、`current_process_fs_context`、`current_resources` 和 `current_futex_key` 只能在当前 task 是用户线程时调用。

## 外部边界 / 攻击面

| 边界 | 来源 | 进入 `kthread` 的形式 | 约束 |
|------|------|------------------------|------|
| syscall 参数 | 用户态系统调用 | PID、TID、fd、futex 地址、timer 参数经 POSIX/syscall 层转换后调用 helper | syscall 层负责用户指针校验、权限检查和 errno 映射 |
| 用户内存地址 | futex、robust list、clear-child-tid | 以 `usize` 地址保存或构造 `FutexKey` | `kthread` 不解引用地址，用户内存访问由 `kuaccess`、`memspace` 或调用方完成 |
| 文件系统和 fd 输入 | POSIX fs/net/ipc 路径 | 通过 `current_resources`、`current_process_fs_context` 获取资源表和路径上下文 | 资源对象和路径解析由 `kresources`、`kfs`、上层 POSIX 模块校验 |
| process capability 输入 | pidfd syscall / clone PIDFD 路径 | `PidFd` 持有 `Weak<ProcessState>` 和 `exit_event` | syscall 层负责 PID 可见性、目标 fd 权限与 errno 映射 |
| signal/timer 输入 | POSIX signal、itimer、POSIX timer | 以 `SignalInfo`、`TimerDelivery`、timer sequence 进入投递路径 | `ksignal` 和 `ktimer` 维护 signal/timer 语义，`kthread` 负责目标 lookup 和 task interrupt |
| TEE runtime state | TEE 子系统 | type-erased `Arc<dyn Any + Send + Sync>` process private slot | 调用方保证同一进程内的具体类型一致 |
| 中断和硬件输入 | timer alarm task 间接触发 | `ktimer` 回调 `poll_timer` 后进入 signal 投递 | `kthread` 入口按 task/runtime 路径设计，不直接读 MMIO、PIO、DMA 或设备内存 |

本 crate 不直接处理 MMIO、PIO、DMA、FFI、inline assembly、bootloader metadata、device tree 或 ACPI 数据。
这些边界由驱动、平台、架构和 boot crate 负责。

## unsafe 代码清单

| 位置 | unsafe 操作 | 不变量 |
|------|-------------|--------|
| `thread/task_ext.rs` | `unsafe impl TaskExt for Box<Thread>` | `Box<Thread>` 作为 task extension 在任务迁移时可移动，线程共享状态均通过 `Arc`、atomic 或锁访问 |
| `thread/task_ext.rs` | `ext.downcast_ref::<Box<Thread>>()` | 用户线程创建时扩展槽写入的具体类型是 `Box<Thread>`，只有该槽被用于 `Thread` 扩展 |

未发现 `transmute`、`from_raw`、`as_mut_ptr`、`UnsafeCell` 或 `MaybeUninit` 使用。
`process_state.rs` 中的 `Weak::as_ptr` 是 safe API，用于 shared futex table 的 region identity，不解引用该指针。

## 内存安全不变量

1. **task extension 类型一致性**：用户线程的 task extension 槽只存放 `Box<Thread>`，`TaskInner::as_thread` 只能按该类型 downcast。
2. **内核任务不伪装为用户线程**：内核任务没有 `Thread` 扩展，进程专用入口在误用时 panic 或返回 `OperationNotPermitted`。
3. **ProcessState 共享所有权**：`Thread` 持有 `Arc<ProcessState>`，同进程线程共享运行态不会被提前释放。
4. **registry 非拥有索引**：全局 task/process/group/session table 只保存 weak entry，不延长对象生命周期。
5. **shared futex identity**：shared futex table key 来自共享映射区域 weak pointer identity，key 只用于哈希索引，不用于内存访问。
6. **timer signal sequence**：POSIX timer signal dequeue 通过 timer id 和 signal sequence 校验陈旧 signal。
7. **TEE 类型擦除**：TEE private runtime state 以 `Arc<dyn Any + Send + Sync>` 保存，取回时必须 downcast 到调用者期望类型。
8. **PidFd 生命周期观察**：`PidFd` 只通过 `Weak<ProcessState>` 观察目标进程，upgrade 失败必须返回 `NoSuchProcess`。

## 线程安全

| 类型或状态 | 并发保护 | 说明 |
|------------|----------|------|
| `Thread::clear_child_tid` | `AtomicUsize` | clone/exit 路径按地址值读写 |
| `Thread::robust_list_head` | `AtomicUsize` | robust futex 路径读取用户链表入口地址 |
| `Thread::time` | `Mutex<CpuTimeStatistics>` | CPU time 采样和状态切换串行化 |
| `Thread::exit` | `AtomicBool` | Release/Acquire 发布退出状态 |
| `Thread::accessing_user_memory` | `AtomicBool` | fault 与 user-memory 访问路径观测 |
| `ProcessRuntimeState::heap_top` | `AtomicUsize` | brk 路径发布和读取 heap top |
| registry tables | `RwLock<WeakMap<...>>` | lookup 并发读，注册与 cleanup 独占写 |
| shared futex tables | `Mutex<FutexTables>` | table 创建和周期清理串行化 |
| TEE private state | `RwLock<Option<Arc<dyn Any + Send + Sync>>>` | 初始化、读取和清理串行化 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | task extension 类型错误导致 downcast UB | 高 | 非 `Thread` 类型写入用户线程扩展槽 | `Thread::new` 创建固定类型；`TaskExt` unsafe impl 限定在 `Box<Thread>` |
| T-02 | 内核任务调用用户线程专用 helper 触发 panic | 中 | kernel task 调用 `current_process_state`、`current_resources` 等入口 | rustdoc 标注 panic 条件；共享路径使用 `current_fs_context` |
| T-03 | registry stale weak entry 导致目标查询失败 | 低 | task/process 已释放但 weak table 尚未 cleanup | lookup 返回 `NoSuchProcess`；提供 `cleanup_task_tables` |
| T-04 | PID 为 0 的 lookup 在内核任务上下文误取当前进程 | 中 | 内核任务调用 `get_process_state(0)` | `get_process_state(0)` 经 `current_process_state` 快速失败 |
| T-05 | signal 目标线程退出竞态导致信号丢失 | 中 | 查到目标后目标退出或 registry entry 过期 | 发送前通过 registry 获取强引用；失败返回 `NoSuchProcess` |
| T-06 | process group signal 遍历成员变化导致部分目标未送达 | 中 | 遍历期间成员退出或 group 关系变化 | 遍历当前可升级进程集合，单目标发送失败向调用者返回错误 |
| T-07 | stale timer signal 被错误投递 | 中 | timer 已更新或取消后旧 signal 出队 | `on_timer_signal_dequeued` 使用 timer id 和 signal sequence 校验 |
| T-08 | shared futex table key 复用导致等待队列串扰 | 高 | weak pointer identity 被复用且旧 table 未清理 | table 周期清理空且无外部引用项；key 不参与解引用 |
| T-09 | TEE private state 类型不匹配 | 中 | 不同调用方以不同类型读取同一 erased slot | downcast 失败返回 `BadState` 或 `None` |
| T-10 | 中断上下文误用锁路径放大延迟 | 中 | IRQ 路径调用 registry、timer manager 或 resource helper | 入口设计面向 task/syscall 生命周期；新增调用点需避免 IRQ 上下文 |
| T-11 | 上层漏校验用户 futex 地址导致错误等待目标 | 中 | syscall 层直接把未校验地址传给 `current_futex_key` | `kthread` 只构造 key，地址合法性和访问权限由 futex syscall 路径负责 |
| T-12 | fd 或路径输入绕过权限控制 | 中 | 上层通过 `current_resources` 取到资源后漏做权限检查 | `kthread` 只返回当前进程资源表，权限和对象类型校验由 POSIX/fs/net 层执行 |
| T-13 | pidfd 在目标进程退出后继续暴露有效 capability | 中 | `PidFd` 错误持有强引用或 poll/upgrade 判断不一致 | `PidFd` 仅保存 weak 引用；`process_state()` upgrade 失败返回 `NoSuchProcess` |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升或跨进程等待队列串扰。
- 中：导致 panic、信号/timer 语义错误、任务生命周期观察错误。
- 低：导致短暂查询失败、统计或展示不一致。

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `as_thread` panic | 当前 task 是 kernel task | 当前调用失败 | syscall/POSIX 路径错误终止 | 2 | rustdoc 标注 panic 条件；共享路径使用 `try_as_thread` 或 `current_fs_context` |
| F-02 | `get_task` 返回 `NoSuchProcess` | task 已退出或未注册 | signal/wait/procfs 查询失败 | 目标进程操作失败 | 3 | clone/init 路径调用 `add_task_to_table`；cleanup 清理过期 entry |
| F-03 | `ProcessState` 初始化字段不完整 | 创建线程时传入错误配置 | 地址空间、fs context 或 signal trampoline 错误 | exec、signal、brk 等路径异常 | 2 | `ProcessStateConfig::default` 使用地址布局常量；创建入口集中传参 |
| F-04 | CPU time 统计偏差 | 线程状态切换漏调 `set_cpu_state` | rusage/procfs/time 展示不准 | 统计类 syscall 偏差 | 4 | 采样时先 update；状态切换由 trap/syscall 路径维护 |
| F-05 | child CPU time 累计溢出 | 长期运行或异常重复累计 | 子进程统计截断 | rusage 展示错误 | 4 | 使用 `usize` nanosecond 计数；调用点应只在 reaped 时累计 |
| F-06 | shared futex stale table 堆积 | 长期没有达到 cleanup 阈值或 table 非空 | 全局 map 增长 | 内存占用上升 | 3 | 每 100 次操作 retain；空且无外部强引用 table 被删除 |
| F-07 | timer observer 未注册 | `init_timer_runtime` 未调用 | timer signal 不投递 | alarm/POSIX timer 失效 | 2 | entry bootstrap 初始化阶段调用；`Once` 保证只注册一次 |
| F-08 | timer dequeue 在非 timer signal 上误处理 | 普通信号进入 observer | 错误 drop signal | 信号语义异常 | 2 | 无 timer id 时返回 `Deliver` |
| F-09 | TEE private state 泄漏或类型冲突 | 调用方未清理或重复初始化不同类型 | TEE 运行态错误 | TEE 会话失败 | 3 | `clear_tee_runtime_private` 清理 slot；类型不匹配返回 `BadState` |
| F-10 | 中断上下文持锁调用 | 错误调用 registry 或 resource helper | IRQ 延迟增加 | 调度和响应延迟 | 2 | 审计清单要求新增调用点标明执行上下文 |

严重度定义：

- 1：致命，系统崩溃或内存破坏。
- 2：严重，线程、signal、timer 或 futex 核心语义不可用。
- 3：一般，目标操作失败或内存占用增长。
- 4：轻微，统计或展示偏差。

## 故障管理

- `get_task`、`get_process_state`、`get_process_group` 和 `get_session` 使用 `KResult` 返回目标缺失。
- `send_signal_to_thread` 对 kernel task 目标返回 `OperationNotPermitted`。
- `send_signal_to_thread` 在 `tgid` 不匹配时返回 `NoSuchProcess`。
- `current_fs_context` 对 kernel task 提供 kernel 默认 context，避免共享路径 panic。
- `init_timer_runtime` 使用 `Once` 注册 timer runtime，避免重复注册 observer。
- TEE private state downcast 失败返回 `BadState`。

## 隐私分析

`kthread` 保存 exe path、cmdline、credential、文件资源表、地址空间引用、线程 ID、CPU time、signal pending 状态和 timer 状态引用。
这些信息会被 syscall、procfs、signal、timer、TTY 和 TEE 路径读取。
本 crate 不直接执行权限检查，调用者需要在上层根据 PID 可见性、credential 和 namespace 规则限制信息暴露。

## 已知限制

- public re-export 面较宽，包含 futex、resource 和 rlimit 类型，主要用于保持既有调用路径稳定。
- registry 依赖 weak entry 和显式 cleanup，查询结果反映当前可升级对象集合。
- shared futex table 使用 weak pointer identity 作为 key，需要依赖周期清理降低 stale key 复用风险。
- `current_thread` 返回 handle 本身不检查 task 类型，实际 deref 时才触发用户线程要求。
- TEE private state 使用单 slot type-erased 存储，同一进程内只能保存一个具体类型的 private runtime state。

## 审计清单

修改本模块时需验证：

- 新增公开 API 有外部调用者，内部 helper 优先保持私有或 `pub(crate)`。
- 新增用户线程专用 API 明确标注 kernel task 上下文的 panic 或错误返回。
- 新增 unsafe 块有 `SAFETY:` 注释，并说明 task extension、生命周期或并发不变量。
- 新增 registry 调用点考虑 stale weak entry 和 PID 为 0 的当前进程语义。
- 新增 signal/timer 路径考虑目标退出、重复投递和 dequeue 竞态。
- 新增 futex 逻辑保持 private table 与 shared table 的隔离。
- 新增 TEE 状态逻辑说明 type-erased slot 的具体类型约束。
- 新增锁调用点标明执行上下文，避免 IRQ 路径调用可能阻塞或放大延迟的 helper。
- 新增 pidfd 相关逻辑说明 `Weak<ProcessState>` 生命周期和 exit event 观察语义。
