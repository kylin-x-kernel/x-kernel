# kprocess — 安全与可靠性分析

## 信任模型

```text
kprocess / posix/process / ksyscall / ktty
   │
   │ safe API: Process, ProcessGroup, Session, init_proc, Pid
   v
┌──────────────────────────────┐
│ kprocess                     │
│                              │
│  process identity graph      │
│  group/session membership    │
│  exit/thread metadata        │
│  per-thread credential refs  │
│  lifecycle wait/exit events  │
│  controlling terminal slot   │
│                              │
│  unsafe boundary: none       │
└──────────────────────────────┘
```

- 调用者负责分配唯一 PID、PGID 和 SID。一般 POSIX 权限检查仍由 syscall 层执行；
  需要统一 task identity 语义的 ptrace-style 跨任务读取由 `kprocess::ptrace` 集中检查。
- `kprocess` 负责在 safe API 内维护父子关系、进程组关系、session 关系和退出状态不变量。
- `kprocess` 负责 current-task credential 的定位和 committed `Arc<Cred>` 发布；凭据转换
  规则由 `kcred` 负责。
- `kprocess` 不解析用户指针，不接收设备 DMA，不处理网络包，不直接读写用户内存。

## unsafe 代码清单

本模块没有 unsafe 代码。
未发现 `unsafe` 块、`unsafe fn`、`transmute`、`from_raw`、`as_mut_ptr`、`UnsafeCell` 或 `MaybeUninit` 使用。

## 内存安全不变量

1. **父子所有权**：父进程 `children` 表持有子进程 `Arc`，子进程 `parent` 字段只保存 `Weak`。
2. **group/session 非拥有索引**：`ProcessGroup::processes` 和 `Session::process_groups` 只保存 weak entry。
3. **init 进程存在性**：普通进程 reparent 依赖 `INIT_PROC` 已初始化。
4. **terminal 对象边界**：`Session::terminal` 保存 `Arc<dyn ControllingTerminal>`，只通过指针相等清除，不在 `kprocess` 内 downcast。
5. **退出回收顺序**：`free` 只能作用于已退出进程，避免 still-running 子进程从父表中被移除。
6. **lifecycle 事件归属稳定**：`child_exit_event` 事件流与 sticky `exit_event`
   completion 归属于 `Process`，不依赖 `ProcessRuntime` 是否仍可升级。
7. **弱 runtime 引用非拥有**：`Process` 只保存 `Weak<ProcessRuntime>`，
   不延长 runtime 生命周期；upgrade 失败时由上层折叠为 `NoSuchProcess` 等语义错误。
   runtime 内的 files、`FsStruct` 和 `NsProxy` owner 可在 runtime 对象仍存活时独立置空，
   capability accessor 必须同时检查对应 owner 是否存在。
8. **live 语义独立于弱 runtime 引用**：外部 `live process` 以 exited state 为准，
   不允许把“runtime 还没释放”误判成“进程仍然活着”。
9. **publication 原子可见性**：task/process/group/session 目录在同一 publication 锁下更新，
   避免 `tgkill(tid)` 已命中而 `kill(pid)` / `pidfd_open(pid)` 仍暂时 `ESRCH` 的跨表半发布状态。
10. **publication 失败必须可回滚**：若 parent-side `CLONE_PIDFD` / `PARENT_SETTID` 等收尾步骤失败，
   staged publication 必须撤销 task/process 目录可见性，以及尚未提交 child 的 parent/group 成员关系，
   不能留下“syscall 失败但 child 仍可见/可 wait”的残留对象。
11. **凭据提交不可见半状态**：`Thread` 只发布不可变 `Arc<Cred>`；checked 转换在普通
   `Cred` 副本上完成后，按 `real_cred`、`cred` 的固定锁顺序同时替换。
12. **objective/subjective 关系明确**：当前未支持 override credential，普通提交要求两个
   旧指针相同，避免静默覆盖未来的临时 subjective identity。
13. **跨任务读取检查集中**：同线程组豁免、objective credential 快照、real-ID 匹配和
    特权绕过由 `ptrace::check_read_real_creds_access()` 一次组合，syscall 不重复拼接字段规则。

## 线程安全

| 类型 | `Send` 条件 | `Sync` 条件 |
|------|-------------|-------------|
| `Process` | 字段均满足 Send | `SpinNoIrq`、atomic state 和 `Arc` 保护共享状态 |
| `ProcessGroup` | 字段均满足 Send | `SpinNoIrq<WeakMap<...>>` 保护成员表 |
| `Session` | 字段均满足 Send | `SpinNoIrq` 保护进程组表和 terminal slot |
| `Thread` credential refs | `Arc<Cred>` 可发送 | 两个 `RwLock` 保护指针替换；`Cred` 本身不可变共享 |
| `ThreadMembership` | 在 `SpinNoIrq` 内使用 | 不直接跨线程共享 |
| `ThreadGroupExitState` | 在 `SpinNoIrq` 内使用 | 不直接跨线程共享 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | PID、PGID 或 SID 冲突导致关系图错误 | 中 | 调用者用已存在 ID 创建进程、group 或 session | `create_session` 和 `create_group` 文档要求调用者先做冲突检查；`posix/process` 通过 registry 检查 group |
| T-02 | 跨 session 移动进程破坏 job-control 隔离 | 中 | 调用 `move_to_group` 时目标 group 属于其他 session | `move_to_group` 比较 `Arc<Session>`，不同 session 返回 `false` |
| T-03 | init 进程退出导致 reaper 缺失 | 中 | 调用者对 init 进程调用 `exit` | `Process::exit` 对 init 进程直接返回 |
| T-04 | 未退出进程被提前回收 | 中 | 调用者对运行中进程调用 `free` | `free` 断言进程已经 exited，错误调用触发 panic |
| T-05 | 控制终端被重复绑定 | 中 | 多个 TTY 尝试设置同一 session terminal | `set_terminal` 返回 `SetTerminalResult::Occupied`；TTY 侧只在短临界区安装已构造 terminal，并在失败时回滚 job-control session |
| T-06 | 错误终端对象清除当前绑定 | 中 | 调用者传入非当前 terminal 的对象调用 `unset_terminal` | `unset_terminal` 使用 `Arc::ptr_eq` 校验对象一致性 |
| T-07 | wait 或 procfs 遍历读到过期 group member | 低 | WeakMap 中存在已释放对象的 weak entry | `ProcessGroup::processes` 通过 `WeakMap::values` 返回可升级对象，registry 另有 cleanup 路径 |
| T-08 | 锁顺序误用导致死锁 | 中 | 外部持有 children、group 或 session 成员锁后调用 group/session mutation API | API 内部统一加锁；新增调用点应避免外层持有 `kprocess` 成员锁 |
| T-09 | group-exit 退出码被普通线程退出覆盖 | 中 | group exit 后其他线程继续调用 `exit_thread` | `exit_thread` 在 `group_exited` 为 true 时不覆盖 `exit_code` |
| T-10 | lifecycle 唤醒仍依赖 runtime-state 查找 | 中 | 退出路径先拿到 parent，却还要回查另一层状态对象 | lifecycle 事件已归属 `Process`，退出路径可直接 wake parent；process exit observer 使用 sticky completion 支持 late pidfd waiter |
| T-12 | 进程 live-state 入口依赖 thread-table 反推 | 中 | PID 可见后仍需通过线程集合和 task table 回查 live state | `Process` 现在直接持有 typed runtime attachment，避免把 task table 当作 live-state 真相 |
| T-13 | 已退出进程因 runtime 尚未释放而被误判为 live | 中 | 退出尾段里当前线程仍强持有 `ProcessRuntime`，但进程已经进入 exited state | `live` 查询只看 exited state；runtime attachment 仅供内部 capability upgrade |
| T-14 | 多目录分步发布暴露 task/process 可见性裂缝 | 中 | parent 已观察到新 tid/pidfd，但 task/process/group/session 目录仍未统一可见 | `ProcessPublication` 用单锁事务同时更新可观测目录；`clone` 在 publication 完成后才回写 `PARENT_SETTID` / `PIDFD` |
| T-15 | staged publication 失败后残留未提交 child | 高 | `clone()` 返回错误，但 child 仍留在 parent.children / thread membership / PID 目录里 | publication handle 默认可回滚；失败时同步撤销目录可见性与未提交 child 关系 |
| T-11 | 中断上下文误用放大关中断区间 | 中 | 在中断上下文中执行进程关系 mutation，或持有 `SpinNoIrq` 后调用长路径逻辑 | `kprocess` API 内部锁区保持短小；新增调用点应限制在 task/syscall 生命周期路径 |
| T-16 | 凭据转换中途被其它检查观察 | 高 | 原地修改共享 credential，或逐字段发布 | prepare/commit 模型只替换完整 `Arc<Cred>`；读取者先克隆快照 |
| T-17 | 下层资源 owner 反向读取 current task 造成层级倒置、身份变化或内核任务 panic | 高 | VFS 路径或匿名文件构造隐式调用 `current_cred()` | current helper 只服务明确的用户 task 入口；syscall 将一个 `Arc<Cred>` 显式传入 `kvfs` 和 fd 对象构造函数 |
| T-18 | 退出进程的大块用户内存释放依赖普通 GC 任务调度 | 高 | fork/exec 风暴中 GC 任务迟迟不运行，已退出进程的地址空间资源堆积 | runtime 持有 `memspace::process_lifetime::MmUserHandle`；最后一个 handle 释放时同步清理 `MmSpace` 的用户映射，普通 `Arc<MmSpace>` observer 或 `MmPin` 不保留映射 |
| T-19 | 父进程显式忽略 SIGCHLD 后 zombie 泄漏或被 wait 抢先回收 | 中 | 父进程设置 `SIGCHLD` 为 `SIG_IGN` 或 `SA_NOCLDWAIT`，child exit 与 parent wait / signal handler 并发 | child-exit 通知先准备 autoreap/queue 决策；autoreap child 跳过 waitable zombie 状态，先撤销 children/PID 身份，再提交 typed SIGCHLD payload，并在提交时按当前线程 mask 选择唤醒目标 |
| T-20 | 失效 PID/TID 目录槽位无限保留 | 高 | wait/exit 只 retire slot 却不从 `BTreeMap` 删除，fork 密集工作负载累积数百 MiB RustHeap | `unpublish_task_if_matches`/`unpublish_process_if_matches` 在 retire 前用 `Arc::ptr_eq` 校验发布身份，再删除仍指向同一 cleanable slot 的目录项；复用后的 Reserved/Published 新身份不会被旧退出路径误退休 |
| T-21 | zombie 或 reaper identity 继续固定 VFS mount | 高 | exited-state 已发布，但 runtime 的 fd table、`FsStruct` 或 `NsProxy` owner 仍存在 | 最后线程先取走 mm/files/fs/ns owner，再发布 exited state；空 owner 的 accessor 返回 `NoSuchProcess` |
| T-22 | ptrace-style syscall 各自实现不一致的凭据比较 | 高 | syscall 逐字段比较 caller/target，错误处理 set-ID 凭据 | `kprocess::ptrace` 集中线程组、real-credential 和特权策略；字段匹配复用 `kcred` 的非对称谓词 |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、权限或 job-control 语义错误。
- 低：导致统计不准、展示过期、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `init_proc` panic | `INIT_PROC` 尚未初始化 | 查询 init 进程失败 | fork/exit/reparent 路径不可用 | 2 | 启动入口先调用 `Process::new_init`；测试 helper 保证 init 存在 |
| F-02 | 子进程 reparent 失败 | init 进程未初始化或 children 锁顺序被外部破坏 | orphan child 留在退出父进程下 | wait 和 procfs 关系错误 | 2 | `Process::exit` 使用 `INIT_PROC` 作为统一 reaper，并在内部按固定顺序更新 children 和 parent |
| F-03 | wait 回收运行中进程 | 调用者绕过 exited-state 检查调用 `free` | 父子关系提前删除 | wait、signal 和 procfs 观察错误 | 2 | `free` 对进程已退出状态做断言 |
| F-04 | setsid 或 setpgid 语义错误 | 调用者未检查 ID 冲突或 session 约束 | 进程组关系错误 | job-control 行为异常 | 3 | `move_to_group` 内部拒绝跨 session；冲突检查由 syscall 和 registry 执行 |
| F-05 | terminal slot 永久占用 | TTY drop 或 ioctl 路径未调用 `unset_terminal` | session 无法绑定新 terminal | TTY job-control 失效 | 3 | `set_terminal` 返回三态安装结果；`TIOCNOTTY` 路径同时调用 `unset_terminal` 和 job-control session 清理 |
| F-06 | WeakMap 残留过期项 | process group 成员释放后索引未清理 | 遍历结果少于表项数量 | 统计或展示短暂不一致 | 4 | `WeakMap::values` 只返回可升级对象；`kprocess` registry 提供 cleanup |
| F-07 | 线程集合统计不准 | 调用者漏调 `add_thread` 或 `exit_thread` | `threads()`、CPU time 和 rusage 统计错误 | procfs、wait、timer 逻辑受影响 | 3 | clone 和 exit 路径集中调用对应 API |
| F-08 | 中断上下文执行进程关系修改 | IRQ 路径误调用 `fork`、`exit`、`create_session` 或 group mutation | 关中断持锁时间变长 | 调度延迟上升，严重时影响系统响应 | 2 | 进程关系修改限定在启动、clone、exit、wait 和 syscall job-control 路径 |
| F-09 | PID/TID publication 目录泄漏 | exit/wait 路径只逻辑失效 slot | RustHeap 随累计 fork 线性增长，buddy 外部碎片 | spawn 类压力测试 OOM | 2 | 热路径按发布身份精确 retire/删除匹配 PID/TID 槽；`cleanup()` 仅作 group/session 兜底 |

严重度定义：

- 1：致命，系统崩溃、内存破坏。
- 2：严重，进程生命周期或 wait 语义不可用。
- 3：一般，job-control 或统计功能异常。
- 4：轻微，短暂展示不一致。

## 故障管理

- `move_to_group`、`unset_terminal` 使用 bool 返回调用是否成功；`set_terminal` 使用 `SetTerminalResult` 区分新安装、同对象重入和被其他 terminal 占用。
- `create_session` 和 `create_group` 在当前进程已经是 leader 时返回 `None`。
- `init_proc` 在 init 尚未初始化时 panic，调用者需保证启动顺序。
- `free` 在目标尚未退出时 panic，调用者需先完成 wait 条件判断。
- 本 crate 不直接返回 Linux errno，errno 映射由 syscall 层完成。

## 隐私分析

`kprocess` 保存 PID、父子关系、线程 ID、退出码、进程组、session、terminal 绑定，
每个线程的 credential 引用、exec path/cmdline metadata 和 OOM score adjustment。
umask 由独立的 `fs_context::FsStruct` 与 root/pwd 一起持有。credential 包含数值
UID/GID 和补充组，但不包含用户名、用户 payload、
文件内容或地址空间内容。上述身份和元数据会被 procfs、wait、
signal、scheduler 和 job-control 路径读取，调用者需要在上层执行可见性和权限控制。

## 已知限制

- subreaper 尚未实现，普通退出进程的子进程统一 reparent 到 init。
- ID 冲突检查不在 `kprocess` 内集中执行，调用者需通过 registry 或 syscall 规则保证唯一性。
- `Session::terminal` 使用 `ControllingTerminal` trait object，`kprocess` 只管理绑定槽，不了解具体 TTY 类型。
- `Process::exit` 不主动从 process group 成员表删除进程，成员表依赖 weak entry 释放和 cleanup。
- 弱 runtime 引用只在 `kprocess` 内部使用，不再作为对外公开的类型擦除桥，也不再作为 `live process` 判据。
- subjective credential override 尚未实现；当前 `real_cred` 与 `cred` 始终共同提交。

## 审计清单

修改本模块时需验证：

- 新增公开 API 是否有外部调用者，内部 helper 优先保持 `pub(crate)`。
- 新增进程关系转换是否保持 parent/children、group/processes、session/process_groups 三组关系一致。
- 新增锁嵌套是否遵循现有 API 内部加锁方式，避免外部持有成员锁后调用 mutation API。
- 新增 task publication 或 rollback 是否通过同一事务对象同时处理全局 TID task slot 和进程内 thread member slot。
- 新增 publication 失败路径是否只 retire 本事务预留的 PID、group、session slot，不撤销既有 published identity，并删除仍可清理的目录 map 项。
- 新增 fork runtime 构造失败路径是否撤销已经 attach 但尚未 publication 的 child relation。
- 新增退出路径是否保持最后线程退出、waitable zombie / autoreap、wait/free 顺序，并在 thread exit / process reap 时携带任务/进程身份按 TID/PID 精确删除目录槽，而非仅凭数字 ID retire（尤其禁止对 `Reserved` 槽位 retire）或依赖全表扫描。
- 新增 child-exit SIGCHLD 行为是否区分默认 ignored、显式 `SIG_IGN` 和 `SA_NOCLDWAIT`，并保持 autoreap 在提交 SIGCHLD pending 和唤醒 parent waiters 前完成。
- 新增凭据修改是否遵循 prepare/check/commit，且失败时不替换 committed `Arc`。
- 新增 ptrace-style 跨任务读取是否复用 `kprocess::ptrace`，而不是在 syscall 层重写凭据比较。
- 需要文件权限的调用是否在 syscall 入口取得一次快照，而不是让下层反向查询 current task。
- 新增 current-thread 尾段路径是否仍可通过稳定 `Process` 访问所需 runtime capability，且不会把已退出进程重新暴露为 live。
- 新增退出 capability 是否在 exited-state 发布前取走；files、fs、namespace accessor
  是否在 owner 已空时拒绝访问，且 owner drop 是否发生在对应 slot 锁外。
- 新增地址空间退出清理是否只在最后一个 runtime `MmUserHandle` 释放时发生，且不得被普通 `Arc<MmSpace>` observer 或 `MmPin` 阻塞或破坏 `CLONE_VM` 共享方。
- 新增用户映射访问路径是否走 live address-space 入口；退出后仅需观察 mm 对象的路径是否显式使用 teardown-observation pinned 入口，避免把 `MmPin` 当成 live user capability。
- 新增 controlling terminal 行为是否保持 set-once 和 pointer-match unset 语义。
