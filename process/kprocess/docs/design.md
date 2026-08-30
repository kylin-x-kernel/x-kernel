# kprocess — 设计文档

## 定位

`kprocess` 提供 x-kernel 的进程身份、线程对象和 POSIX job-control 基础对象。
在最新设计决议中，它是唯一的 process domain owner crate。
它维护 process、thread、process group、session、父子关系、线程组成员、退出状态、
publication/registry、signal targeting、timer delivery glue、controlling terminal 绑定。
当前 `Process` 自身保留一个强类型的弱 runtime 引用
（`Weak<ProcessRuntime>`），并直接对外提供 fs/mm/signal/timer/credential
等 capability 方法；每个 `Thread` 则持有自己的 objective/subjective credential
指针。timer capability 通过 `Process` facade 暴露单个语义操作，外部模块不直接持有
process-owned timer manager 的锁。外部 `live process` 语义不再以弱 runtime 引用为判据，
而是以 `Process` 生命周期状态是否已经 exited 为准。
`current_user_*` helpers 显式要求当前 task 必须已经安装 user-thread runtime。
`posix/process`、`ksyscall`、`ktty`、procfs 等模块只通过 `Process` 公开 API
或 `process_exit` / `wait_reap` 语义模块查询或调整进程关系与能力。

## 背景

x-kernel 需要在内核中表达 POSIX 进程层级、进程组、session 和终端控制关系。
这些关系跨线程调度、信号、wait、TTY job control 和 procfs 展示共用。
`kprocess` 当前已经同时拥有：

- `Process`
- `ProcessRuntime`
- `Thread`
- publication / registry / signal / timer glue
- lookup / domain-facing query modules

## 范围

涉及的源文件：

```text
process/kprocess/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── capability.rs
    ├── cgroup.rs
    ├── credentials.rs
    ├── ptrace.rs                 # ptrace-style 跨任务访问策略
    ├── job_control.rs
    ├── lookup.rs
    ├── pidfd.rs
    ├── process.rs
    ├── process/
    │   ├── exit.rs
    │   ├── runtime_access.rs
    │   ├── thread_membership.rs
    │   └── tree.rs
    ├── process_exit.rs
    ├── process_signals.rs
    ├── process_group.rs
    ├── process_runtime/
    │   ├── mod.rs
    │   ├── posix_state.rs
    │   └── runtime_state.rs
    ├── procfs.rs
    ├── publication.rs
    ├── resource_limits.rs
    ├── scheduler.rs
    ├── session.rs
    ├── signal.rs
    ├── stat.rs
    ├── system_view.rs
    ├── thread/
    │   ├── cpu_time.rs
    │   ├── core.rs
    │   ├── current.rs
    │   └── task_ext.rs
    ├── timer_delivery.rs
    └── wait_reap.rs
    
```

## 架构

```text
Process
  ├─ children: intrusive List<Arc<ChildRelationSlot>>
  ├─ parent_relation: ParentRelation
  │    ├─ wait_contract: WaitParentContract { Weak<Process>, exit_signal }
  │    └─ current_child_slot / init_reparent_slot
  ├─ thread_membership: ThreadMembership
  │    └─ members: BTreeMap<Tid, Arc<ThreadMemberSlot>>
  ├─ group_exit: ThreadGroupExitState
  ├─ lifecycle: ProcessLifecycleState
  │    ├─ events: ProcessEvents
  │    └─ cpu_totals: ProcessCpuTotals
  ├─ group_membership: GroupMembership
  │    └─ group / member_slot
  ├─ pid_publication_slot
  └─ runtime_ref: Option<Weak<ProcessRuntime>>
              │
              v
        ProcessRuntime
          ├─ mm_user: Option<MmUserHandle>
          ├─ resources.fd_table: Option<Arc<RwLock<FdTable>>>
          ├─ fs_context: Option<Arc<Mutex<FsStruct>>>
          └─ nsproxy: Option<Arc<NsProxy>>
              ^
              │
        Thread
          ├─ process: Arc<Process>
          ├─ runtime: Arc<ProcessRuntime>
          ├─ real_cred: RwLock<Arc<Cred>>
          └─ cred: RwLock<Arc<Cred>>

        ProcessGroup
          ├─ processes: BTreeMap<Pid, Arc<ProcessGroupMemberSlot>>
          └─ session: Arc<Session>
                          │
                          v
                    Session
                      ├─ process_groups: WeakMap<Pid, Weak<ProcessGroup>>
                      └─ terminal: Option<Arc<dyn ControllingTerminal>>
```

| 组件 | 职责 |
|------|------|
| `Process` | 保存 leader `PidHandle`、父子关系、线程成员表、线程组退出状态、lifecycle 事件、已退出线程/已回收 child CPU time、退出状态、当前进程组、弱 runtime 引用，以及对外 capability 入口 |
| `ProcessPublication` | 保存 published task/process/group/session 的全局可观测目录，并承载 publish / unpublish / lookup / iteration；目录更新在单次 publication 事务内完成，避免 task/process/group/session lookup 读到跨表半发布状态；其中 `published` 覆盖 waitable zombie 未 reap 的稳定身份，`live` 仅表示尚未 exited 的外部可操作进程 |
| `kidentity` | 作为 process domain 的 identity owner，维护底层 `PidHandle` 与 `PidNamespace`；`kprocess` 当前对外仍暴露 root/global `Pid/Tid` 语义 |
| `lookup` | `kprocess` 内部目录原语层，负责 `published/live` 合约下的 task/process/group 查找 |
| `cgroup` | cgroup adapter facade；负责稳定 task identity 到 published process 的映射、事务内授权回调和整组迁移 |
| `procfs` / `scheduler` / `job_control` / `pidfd` / `process_signals` / `resource_limits` / `process_exit` / `wait_reap` / `system_view` | 面向外部领域语义的窄接口层；外部模块通过这些模块表达“要做什么”，而不是自己理解 `published/live` |
| `ParentRelation` | 封装 wait parent contract、当前 parent child-list slot 和预留 init reparent slot；这些状态只能在 process-domain 事务内作为同一父子关系更新 |
| `GroupMembership` | 封装当前 `ProcessGroup` 和发布到 group 成员表的 slot；group move 通过该对象保持 group 指针与 member slot 成对切换 |
| `ProcessLifecycleState` | 聚合进程生命周期事件和 CPU totals；内部 `ProcessEvents` 保存 child exit 事件流和 process exit completion，`ProcessCpuTotals` 保存已退出线程与已回收 child CPU time |
| `ThreadMembership` | 保存进程自有的 published 线程成员表（`tid -> weak task`） |
| `ThreadGroupExitState` | 保存进程退出码和 group-exit 标志，独立于线程成员表 |
| `Thread` | 保存 task identity、所属 process/runtime、objective `real_cred`、subjective `cred` 和线程私有状态 |
| `ProcessGroup` | 保存 PGID、所属 session 和弱引用进程成员表 |
| `Session` | 保存 SID、弱引用进程组表和 controlling terminal |
| `INIT_PROC` | 保存全局 init 进程，供退出 reparent 和 `init_proc` 查询 |

## 状态机

### Process 生命周期

```text
Created ──publish_task──> Running ──exit_thread(last)──> Exiting
   │                                              │
   │                                              ├─default wait policy──> Zombie ──wait/free──> Dead/Reaped
   │                                              │
   │                                              └─autoreap────────────> Dead/Reaped
   └────────────────────────────────────────────────────────────────────> Dead/Reaped
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| `Created` | `Running` | publication 阶段把已准备好的 task 发布到 `Process` 自有线程成员表 |
| `Running` | `Running` | 非最后一个线程调用 `exit_thread` |
| `Running` | `Exiting` | 最后一个线程离开线程成员表，开始释放进程 owner |
| `Exiting` | `Zombie` | owner 已释放，且 child-exit 策略要求父进程 wait |
| `Exiting` | `Dead/Reaped` | owner 已释放，且 child-exit 策略要求 autoreap |
| `Zombie` | `Dead/Reaped` | 父进程 wait 路径调用 `free` 或 `wait_reap` 获得单赢家 |

`Exiting` 是最后一个线程移除后、退出状态发布前的控制流阶段，不增加独立的状态字段。
`Process::exit` 对 init 进程直接返回。
普通进程退出时设置退出状态，并把子进程 reparent 到 init 进程。
`free` 只允许已退出进程调用，并从父进程 children 表移除当前进程。

### Process group 和 session 转换

```text
Inherited group
  ├─create_group───> New process group in same session
  ├─create_session─> New session + new process group
  └─move_to_group──> Existing group in same session
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| 继承父进程 group | 新 group | `create_group` 且当前进程不是 group leader |
| 继承父进程 session | 新 session 和新 group | `create_session` 且当前进程不是 session leader |
| 当前 group | 指定 group | `move_to_group` 且目标 group 属于同一 session |

## 算法流程

### 创建 init 进程

1. owner 为 init task 分配 leader `PidHandle`，保证首个 root-visible leader 为 `pid/tid 1`。
2. 创建新的 `Session` 和 `ProcessGroup`，并建立稳定 init `Process` 身份。
3. 创建 init `Thread` 与 `ProcessRuntime`，并把弱 runtime 引用登记到该 `Process`。
4. 用同一个 `PidHandle` 和上一步的 `Thread` 通过 `new_user(...)` 构造一个全新的 `User` 身份 task（runtime 在构造时一次性装入 `UserRuntimeSlot`）；完成 tty / stdio 等外部 owner 设置后，经 `publish_user_task(task).commit(...)` 发布并激活。
5. process publication 在进入用户态前完成，使 PID/TID 与 group/session lookup 对外一致可见。

### fork 子进程

1. `fork_process_runtime` 先按 `ForkFs` 准备 child filesystem context，并按
   `NamespaceFlags` 克隆 child `NsProxy`；共享 fs 场景若 parent 正处于 exec
   临界状态会返回 `WouldBlock`。
2. 当前 `CLONE_NEWPID` 尚未启用，因此 leader `PidHandle` 仍在 root PID namespace
   分配，并保持 root-visible `tid == pid`；后续 task-active PID namespace 应挂在
   task/PID identity 层，而不是 `NsProxy`。
3. 再创建子进程稳定身份、预分配 child relation slot，并加入 group 的 weak
   member 表。`ProcessForkConfig` 使用 `ForkParent`、`ForkAddressSpace`、
   `ForkFs`、`ForkSignalActions` 和 `ForkFdTable` 表达 clone 语义，syscall 层
   负责把 Linux flags 解码成这些 typed config。parent link、父进程 children 链表挂接和 `CLONE_PARENT` 的
   parent/exit-signal 继承在同一个 process-domain 临界区完成；普通 fork/clone
   使用调用方请求的 exit signal，`CLONE_PARENT` 子进程继承调用者进程自身的
   exit signal 契约，而不是使用本次 clone 请求中的 signal。
4. tree attach 后，`fork_process_runtime` 使用 RAII rollback guard 覆盖后续
   fallible 准备窗口；随后按 `ForkAddressSpace` 准备私有/共享地址空间状态，
   按 `ForkSignalActions` 准备私有/共享 signal action 表，最后创建
   `ProcessRuntime`、继承 oom score / heap top，并按 `ForkFdTable`
   安装共享或克隆的 fd table。若 runtime 输入准备失败，guard 会撤销未发布 child
   的 parent/group 关系，避免 parent children 残留不会运行的半构造进程。
   umask 不属于 `ProcessRuntime`：它与 root/pwd 一起由前一步准备的 `FsStruct`
   共享或复制，因此 `CLONE_FS` 自动共享 umask。
5. child `Thread` 取得调用线程 subjective credential 的同一 `Arc<Cred>` 快照，并分别安装为自己的 `real_cred` 和 `cred`。
6. syscall 层通过 staged publication 事务先完成 publication，使 PID/TID 与 group/session lookup 对外一致可见；随后执行 `CLONE_PARENT_SETTID` / `CLONE_PIDFD` 等父侧 writeback，成功后才把 task 变为 runnable，失败则回滚 publication 与 owner-side child membership。顺序对齐 Linux `kernel_clone()` 的“先完成 parent-side return setup，再 `wake_up_new_task()`”约束。

### exec 状态发布

`apply_exec_update` 先升级一次 `ProcessRuntime` 并完成 heap、signal actions、
POSIX timer、cloexec fd、TEE/TIPC 私有状态等 post-exec cleanup，最后才
发布新的 `(exe_path, cmdline)` metadata 快照。metadata 使用同一个
`ExecMetadata` RwLock 保存，因此 procfs 等观察者不会看到旧 path + 新 argv 或新
path + 旧 argv 的混合状态。

### 创建同进程线程

1. process-domain owner 为 sibling thread 分配新的 thread `PidHandle`。
2. 新 `Thread` 复用所属 `Process` 与 `ProcessRuntime`，并要求 task-owned identity 与 thread identity 保持一致。
3. 新 `Thread` 初始共享调用线程的 committed credential `Arc`，之后任一线程提交新凭据时只替换自己的两个指针。
4. `prepare_user_task()` 在发布前校验 task identity / thread identity 一致性。
5. publication 在单次 owner-side 事务里同时更新 task/process/group/session 可见性，并把 task 挂入 `Process` 自有线程成员表；事务对象在 activation 前若失败或被丢弃，会自动回滚 task/process 可见性以及未提交 child 的 owner-side 成员关系。可观测方应通过语义 helper 查询 published thread，而不是扫描全局 task 目录反推成员关系。

### 创建 session

1. `create_session` 检查当前进程所属 session 的 SID 是否等于 PID。
2. 创建 SID 等于 PID 的新 `Session`。
3. 创建 PGID 等于 PID 的新 `ProcessGroup`。
4. `set_group` 从旧 group 移除当前进程，再加入新 group。
5. 返回新 session 和新 group。

### 进程退出和回收

1. `exit_thread` 从 `Process` 自有线程成员表移除 TID，按任务身份从全局 TID
   目录 unpublish，并在未 group-exit 时记录退出码。此后到 `set_exit`/任务销毁
   之前，外部 `lookup::task(tid)`、`tgkill`、按 TID 的定时器投递都会看到
   `NoSuchProcess`；这是有意的可见性取舍，避免退出尾段仍被当作可命中目标，
   并防止 Published(dead Weak) TID 槽位滞留。
2. 最后一个线程先释放 `MmUserHandle`，完成共享内存和进程私有状态清理，再依次
   从 runtime 取走 fd table、`FsStruct` 和 `NsProxy` owner。每个 owner slot 都可
   独立置空，重复释放安全返回；后续 capability 查询返回 `NoSuchProcess`。
3. owner 释放完成后，`posix-process` 才通过 `process_exit` 发布稳定 `Process` 的
   exited state。`Process` 内部在 process-domain 临界区内设置 `Zombie` 或 `Dead`，
   将所有子进程 reparent 到 init，并同时更新旧 parent children、新 parent
   children、child parent link 和 orphan 的退出通知 signal。reparent 会把
   orphan 的退出通知 signal 重置为 `SIGCHLD`，避免 init 继承非 `SIGCHLD`
   clone-child 通知语义。
4. 默认 SIGCHLD 语义下，waitable zombie 之后该 `Process` 仍保持 published 身份，继续承担 wait/pidfd/reap 语义；与此同时，外部 `live` 查询必须开始把它视为不可操作对象。
5. 弱 runtime 引用不在 zombie 转换时主动清除，但 runtime 中的 mm/files/fs/ns
   owner 已经置空。当前退出线程可继续持有 runtime 壳完成尾段，zombie/reaper identity
   不会因此继续固定 VFS path 或 mount。
6. 当退出线程释放的是最后一个 active mm user 时，同步释放 VMA 和用户页资源。
   共享 VM 场景通过从父 runtime 的 handle 派生新 user 继续持有，普通
   `Arc<MmSpace>` observer 或 `MmPin` 不参与该判定。
7. 退出路径在父进程可观察前，先把当前线程最终 CPU time 累计到 `ProcessLifecycleState`，并通过本进程 `exit_event` 通知 pidfd/poll 观察者。
8. child-exit 通知对齐 Linux `do_notify_parent()`：默认忽略的 SIGCHLD 仍会排队；显式 `SIG_IGN` 或 `SA_NOCLDWAIT` 请求 autoreap。`SA_NOCLDWAIT` 保持 Linux 行为，除非同时显式 `SIG_IGN`，否则仍发送 SIGCHLD。发送给父进程的 signal 使用 child-exit `siginfo_t` layout，而不是普通 `SI_KERNEL`：`si_code` 从 wait status 映射为 `CLD_EXITED`、`CLD_KILLED` 或 `CLD_DUMPED`，`si_status` 携带退出码或终止信号，并填充 child PID、real UID、用户态/内核态 CPU clock ticks。非 SIGCHLD 的 clone exit signal 也沿用同一 child-exit payload，只替换 `si_signo`。
9. 对 SIGCHLD 退出，运行时先在 process-domain read side 采样
   `(parent, exit_signal)`，随后在锁外读取 child-exit action 并得到
   autoreap/queue 决策，但延迟把 SIGCHLD 放入 pending queue。最终进入
   process-domain write side 提交 `Running -> Zombie/Dead`、autoreap detach 和
   reparent；提交前重新校验 parent/exit-signal contract，若父进程并发退出导致
   reparent，则丢弃旧 signal 准备结果并重新按当前 parent 准备。释放
   process-domain 锁后才提交 SIGCHLD，并在提交时按父进程当前线程和 signal mask
   重新选择打断目标；随后注销 published PID 身份并唤醒 `child_exit_event`，使
   signal handler 和阻塞的 wait 都只能观察到已完成的 exit/autoreap 状态，同时避免
   process-tree 写事务嵌入 signal 子系统锁。SIGCHLD 提交流程只接受 typed
   child-exit SIGCHLD payload，非 SIGCHLD 的 clone exit signal 转换为普通
   process-directed signal 发送。
10. wait 路径通过 `wait_reap::scan_waitable_child` 执行 typed wait 事务：
    syscall 层只把 ABI 选项转换成 `WaitChildSelector`、`WaitChildKind` 和
    `WaitReapMode`，child 匹配、waitable 判断、`Zombie -> Dead` 状态转换和
    parent children 移除在同一个 process-domain 临界区完成。事务返回稳定
    `WaitedChild` 后，锁外再注销 PID identity、累计 child CPU time 和写用户
    exit status，避免 syscall 层持有 stale children snapshot 后自行 reap。

## 并发模型

- process-domain 临界区保护所有跨进程树和全局可见性关系的不变量：child parent
  link、parent children 集合、exit state 的 Running/Zombie/Dead 转换、fork
  挂载、exit reparent、wait reap detach、autoreap detach，以及 task/process/
  process-group/session publication slot 的可见值切换。调用方不得在该临界区之外把
  “判断当前状态”和“修改父子关系/退出状态/可见性状态”拆成两个动作。
- 该边界对齐 Linux `tasklist_lock` 的职责。Linux 使用 IRQ-safe rwlock：
  fork/exit/reparent 等写路径持 write lock，wait 扫描持 read lock，并在
  `EXIT_ZOMBIE -> EXIT_DEAD` 获得单赢家后释放 tasklist lock。x-kernel 的
  `process_domain` 使用 `kspin::SpinRwNoIrq`：纯 parent/children/exit-signal
  快照走 read side，fork/exit/reparent/autoreap/reap/detach 这类会修改
  parent link、children 集合或 exit state 的事务走 write side。当前 reap
  会在消费 zombie 时直接从 parent children intrusive list 摘除 relation
  slot，因此不能简单照搬 Linux 在 read side 下 claim zombie 的分阶段实现；
  若后续拆出 Linux 风格 `release_task` 阶段，可在同一抽象层把 wait 扫描扩大为
  read-side 事务。
- 核心 parent-child relation mutator 接收 `ProcessDomainWriteGuard` token，
  例如 attach、detach、exit reparent、wait reap 和 unpublished rollback 路径。
  group membership、thread membership 和 PID publication helper 也接收同一
  token。这把原先只靠 `_locked` 命名表达的前置条件推进到 Rust 类型检查。
- `ksync::RwLock` 是 sleepable lock，会在竞争时 `block_on()`，不能用于这些
  禁止睡眠的 exit/wait 临界区。
- `Process::children` 使用 intrusive `ChildRelationSlot`，slot 在 `Process`
  构造时分配；process-domain 锁内只把已经存在的 slot 挂入/摘出 parent 的
  children list，并更新 slot 的 child 值、child parent link 与当前 parent slot。
  exit reparent 直接 drain dying parent 的 children list 并挂到 init 预留 slot，
  不在 process-domain write lock 内分配临时 `Vec`。这对齐 Linux
  `list_add_tail(&p->sibling, &p->real_parent->children)` /
  `list_splice_tail_init()` 的“不在 tasklist_lock 下分配”约束。
- `ProcessPublication` 是 task/process/group/session 的全局观察目录，职责对齐
  Linux 在 `tasklist_lock` 下维护 PID hash 和全局 task 可见性的部分。当前实现
  采用 staged slot registry：`BTreeMap` 只保存 slot，结构性插入/移除在
  process-domain 锁外完成；slot 有 `Vacant/Reserved/Published/Retired` 生命周期。
  热路径按发布身份精确删除：`exit_thread` 携带退出 `KtaskRef` 调用
  `unpublish_task_if_matches`，wait/autoreap/`reap_detached_process_identity`
  一律走 `unpublish_process_if_matches`，不再提供仅凭数字 PID 的 unpublish 路径。
  retire 前用 `Arc::ptr_eq` 校验槽内发布对象仍是本次退出的 task/process；
  `Reserved` 在途 republish 不会被 retire；只有目录仍指向同一 slot 且 slot 处于
  `Vacant/Retired` 时才 `BTreeMap::remove`。因此 PID/TID 复用后已 Reserved 或
  Published 到新身份的槽不会被旧退出路径误退休或误删。`cleanup()` 仍只清理
  `Vacant/Retired` 与失效 weak entry，作为 group/session 与异常 abort 路径的
  兜底，不能删除 reserved-but-empty slot，因此 reserve 与 visible commit 之间
  即使发生 cleanup 也不会丢失发布位置。
  publication table write guard 覆盖 reserve 到 commit/abort 的窗口，避免多个
  事务复用同一个 Reserved group/session slot；进入 process-domain 前已完成所有
  `BTreeMap` 结构性分配，slot 的 published value 在 process-domain 锁内切换，
  因此 fork publish、rollback、wait reap 和 autoreap 的对外可见性提交不在
  IRQ-off 临界区内分配内存。
  后续若引入 intrusive hash/list，也应保持同样的 prepare-may-fail /
  commit-noalloc 边界。
- `Process::thread_membership.members` 和 `ProcessGroup::processes` 使用 staged member
  slot：结构性 `BTreeMap` slot reserve 在事务前完成，对外枚举只返回 published
  slot。task publication 用 `ThreadPublicationBinding` 把全局 TID task slot 和
  进程内 thread member slot 绑定为同一事务对象；process-domain commit 同时发布
  两个观察面，rollback 同时 retire 两个 slot 并从目录移除仍可清理的 map 项。
  PID identity 是否由本次 task publication 插入使用 `ProcessIdentityEffect` 表达，
  rollback 只撤销本事务插入的 process identity。group member 也只在
  process-domain 内发布；group/session lookup slot 的本次预留状态由
  `PublicationSlotEffect` 记录，失败路径只 retire 本事务预留但未发布的 slot。
  rollback/reap/autoreap retire 并精确删除匹配 slot，使未发布 child 或已回收
  child 不会在 thread/process 目录中无限滞留。
- job-control group move 通过 `ProcessPublication::move_process_to_group` 统一提交：
  先在 process-domain 外预留目标 process-group member slot 与 group/session
  publication slot，然后在同一个 process-domain 写事务里更新 `Process::group`、
  retire 旧 member、publish 新 member，并发布 group/session lookup 身份。跨
  session move 校验也在该事务内完成；失败路径 retire 本次预留但未发布的 slot。
- process-domain 锁内只允许短小的非阻塞 owner-side 操作；PID publication
  注销、signal commit、task interrupt 和 `PollSet` wake 等外部可观察动作在锁外执行。
- process-owned timer state 由 `ProcessRuntimeState` 内部的 `ProcessTimers`
  子对象保存，但 `Process` 只向外部暴露 create/get/set/delete/poll/dequeue 这类
  timer facade。syscall 与
  signal delivery 路径不直接获取 `ProcessTimerManager` 或其 mutex，避免把 timer
  锁顺序和 lock scope 泄漏成跨模块契约；可能触发 signal delivery 的方法只返回
  `TimerDelivery`，实际派发在 timer manager 锁外完成。
- `timer_delivery::poll_cpu_timers` 和 POSIX timer signal dequeue hook 只能从当前
  user-thread 返回/信号处理路径调用；它们会读取当前 user thread 的 live runtime。
- `Process::thread_membership`、`group_exit`、`children`、`parent_relation` 和
  `group_membership` 内部使用 `SpinNoIrq`，避免持锁区被本地中断打断。
- `ParentRelation` 保护 wait-parent weak reference、exit signal、当前 parent
  child-list slot 和预留 init reparent slot；读取或更新 parent link 时必须先进入
  process-domain 临界区，避免观察到“新 parent + 旧 signal”或“旧 parent + 新
  signal”的组合。
- `Process::runtime_ref` 使用 `SpinNoIrq<Option<Weak<ProcessRuntime>>>`，只保存非拥有型 runtime upgrade 入口，不承担对外 `live` 判定。
- `kidentity` 当前按 namespace 线性分配 `PidHandle`；当前阶段不做回收，后续如引入 pid reuse，应仍保持 “publish before runnable” 不变量。
- `PidHandle` 已能携带 namespace 链，但 `kprocess` 对外 `pid()/tid()` 仍固定返回 root/global 编号；在 wait、kill、procfs、registry 全部 namespace-aware 之前，不把 namespace-visible 编号暴露到对外主语义。
- `ProcessLifecycleState` 的 `child_exit_event` 通过 `Arc<PollSet>` 表达父进程可连续观察的 child-exit 事件流；单个 process 自身的
  `exit_event` 通过 `Arc<kpoll::Completion>` 表达 sticky completion，供 pidfd 和其它 late observer 注册。已退出线程和已回收 child CPU time
  对外统一使用 `TimeSpan`；`ProcessCpuTotals` 仅在内部以 relaxed `u64` 纳秒原子保存表示，加载后立即恢复为语义类型。
- `ProcessGroup::processes` 和 `Session::process_groups` 使用 `WeakMap`，避免 group/session 成员表延长成员生命周期。
- `Thread::real_cred` 与 `Thread::cred` 分别使用 `RwLock<Arc<Cred>>`；读路径只克隆 `Arc`，写路径按固定顺序同时替换两个指针。
- `Session::terminal` 使用 `SpinNoIrq<Option<Arc<dyn ControllingTerminal>>>`，controlling terminal 只能设置一次；`set_terminal` 只在短临界区内 compare-and-install 已构造好的 `Arc`，清除时要求对象指针匹配。`kprocess` 只持有 controlling-terminal trait object，不了解具体 TTY 类型；需要 downcast 的 `/dev/tty` 解析留在 `ktty` 边界完成。
- `exit_state` 使用 `AtomicProcessExitState` 表达 `Running` / `Zombie` / `Dead`，raw 编码被封装在 typed atomic 内。单字段查询仍用
  Acquire 读取；与 parent/children 关系有关的状态转换必须在 process-domain 临界区内完成。
- `set_group` 同时修改当前 process、旧 group 和新 group，调用者应避免在外部持有这些对象的成员锁后再调用 group mutation API。

## 设计决策

### 关系对象独立于运行时资源

`kprocess` 只维护进程身份关系和生命周期摘要。
地址空间 capability 留在 `kprocess` runtime，文件描述符和信号处理
分别由 `kresources`、`ksignal` 拥有；credential 的数据与转换策略由 `kcred` 定义，
task 级 committed credential 指针由 `Thread` 拥有；non-PI futex waiter 由 `kfutex`
全局固定 bucket 独立拥有，不再保存 process-owned futex table。
这种分层让 job-control 关系能够被多个上层模块共享，也避免 `kprocess` 变成进程运行时状态集合。
保留弱 runtime 引用是为了让稳定 `Process` 对象自己承载“升级到运行态 capability”的入口，
而不是让外部再依赖额外 trait 或第二层全局 registry 真相。

### 凭据归属于 task

Linux 的 `task_struct` 同时持有 objective `real_cred` 与 subjective `cred`。x-kernel 将
这两个引用直接放在 `Thread`，而不是 `ProcessRuntime` 或文件系统对象中。
`current_cred()` / `current_real_cred()` 只负责从当前 `Thread` 克隆 committed `Arc`；
`prepare_creds()` 返回未提交副本，`commit_creds()` 在转换成功后原子替换当前线程的
两个指针。当前没有 override credential，因此提交时要求两个旧指针相同。

VFS 处于 `kprocess` 下层，不能调用这些 current helpers。syscall 入口取得一次快照后
显式传入 `kvfs`。匿名 fd 的资源 owner 同样接收显式 `Arc<Cred>`，pidfd 构造也由
syscall adapter 提供快照；credential 最终只保存在 `VfsFile::f_cred`，不复制进资源对象。
这样既保持单向依赖和一次操作的身份一致性，也允许内核任务明确选择其凭据。

### 跨任务读取权限

`ptrace::check_read_real_creds_access()` 集中组合 Linux
`PTRACE_MODE_READ_REALCREDS` 风格的 task-level 策略：同一 `Process` 的线程直接放行；
跨进程时分别取得 caller/target 的 objective credential 快照，复用 `kcred` 的非对称
real-ID 匹配谓词，并以当前 `euid == 0` 特权模型近似 `CAP_SYS_PTRACE`。syscall adapter
只负责解析目标并调用该策略，避免各 syscall 重复实现字段比较和线程组判断。

### 查询策略层与领域接口层分离

`published` 和 `live` 仍然是 `kprocess` 内部必须保留的两类 contract：

- `published` 用于可观测身份目录
- `live` 用于对外仍可操作、尚未 exited 的进程

但这两个术语不再作为外部主编程模型暴露。当前实现采用两层结构：

1. `lookup`
   - 只承载 crate 内部目录原语
   - 统一持有 `published/live` 判定语义
2. 领域模块
   - 如 `procfs`、`scheduler`、`job_control`、`pidfd`、`process_signals`
   - 对外暴露按场景命名的窄接口

这样可以同时避免两种坏味道：

- 外部模块直接理解 `published/live`
- `kprocess::lib.rs` 根命名空间堆满按调用方枚举的 helper

### group/session 成员表使用弱引用

process group 和 session 是查询索引，不拥有成员进程或成员进程组。
成员表使用 `WeakMap`，生命周期由进程父子关系、任务表和外部强引用决定。
这种设计降低循环引用风险，代价是查询时需要清理或过滤过期 weak entry。

### init 进程承担 reaper 角色

当前实现把退出进程的子进程统一 reparent 到 init。
这满足基本 wait 和 orphan child 处理需求。
subreaper 尚未实现，代码保留 TODO。

## Cgroup membership 与 namespace staging

每个 `Thread` 持有唯一的 `TaskMembership`。fork/clone 在任何可失败的 runtime 构造前
预留 pids charge。`Process` 的 cgroup transaction gate 串行化 membership 选择、
whole-process migration 和线程 publication：迁移更新已发布线程与 process target；稍后
发布的 prepared sibling 会先迁移到该 target，再进入 task/thread registry；迁移失败时
publication 返回错误，task 不会变为可见。因此稳定
线程组不会被 fork/migrate 竞态拆散。线程退出在发布 process-exit 状态前显式 detach，
`Drop` 仅是兜底。

`cgroup.rs` 是文件系统 adapter 与 process internals 之间的稳定 facade。
`cgroup_member_process_ids()` 只接受 canonical `Cgroup`，对每个 membership 的
`Arc<PidHandle>` 执行 registry lookup 和指针 identity 校验，再返回去重后的 PID；
它不会把 stale 数值 TID 映射成复用后的新 task。`migrate_cgroup_process()` 集中执行
PID 解析和整组 membership transaction，并在 process cgroup gate 内把稳定 source、
destination 交给文件系统提供的 authorization closure。`cgroup2fs` 因此可使用打开文件
时保存的 credential 和 mount view 做授权，同时不依赖 `scheduler` 或 `procfs` 的内部
查找接口，也不反向查询 ambient current credential。

`CLONE_NEWCGROUP` 在统一 user-namespace capability 授权接入前返回 `ENOSYS`；namespace
staging 不接收或重新查询当前 cgroup 来绕过该授权边界。

## Drop / 资源释放

- `Process` 没有自定义 `Drop`，生命周期由 `Arc` 引用计数控制。
- 父进程 children 表持有子进程强引用，`free` 或 autoreap 从父表移除已退出子进程后释放这条所有权边。
- 用户进程的大块地址空间资源不依赖 `ktask` GC；最后一个 runtime `MmUserHandle` 释放时会同步清理 `MmSpace` 的用户映射。
- fd table、`FsStruct` 和 `NsProxy` 不等待整个 `ProcessRuntime` drop；最后线程在
  exited-state 发布前取走对应 owner。共享对象由最后一个 `Arc` owner 自然释放。
- 需要访问 live 用户映射的路径通过 `Process::address_space()` 进入，该入口要求
  runtime 仍能派生 active `MmUserHandle`；退出清理后需要观察稳定 mm identity 或
  空 VMA 状态的内部路径必须显式使用 teardown-observation pinned address-space 入口。
- `ProcessGroup` 和 `Session` 成员表持有弱引用，不阻止成员释放。
- `Session::unset_terminal` 只在传入 terminal 与当前 terminal 指针相同的时候清空绑定。
