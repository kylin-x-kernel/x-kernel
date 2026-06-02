# kprocess — 安全与可靠性分析

## 信任模型

```text
kthread / kservices / posix/process / ksyscall / ktty
   │
   │ safe API: Process, ProcessGroup, Session, init_proc, Pid
   v
┌──────────────────────────────┐
│ kprocess                     │
│                              │
│  process identity graph      │
│  group/session membership    │
│  zombie and thread metadata  │
│  controlling terminal slot   │
│                              │
│  unsafe boundary: none       │
└──────────────────────────────┘
```

- 调用者负责分配唯一 PID、PGID 和 SID，并在 syscall 层执行 POSIX 权限检查。
- `kprocess` 负责在 safe API 内维护父子关系、进程组关系、session 关系和退出状态不变量。
- `kprocess` 不解析用户指针，不接收设备 DMA，不处理网络包，不直接读写用户内存。

## unsafe 代码清单

本模块没有 unsafe 代码。
未发现 `unsafe` 块、`unsafe fn`、`transmute`、`from_raw`、`as_mut_ptr`、`UnsafeCell` 或 `MaybeUninit` 使用。

## 内存安全不变量

1. **父子所有权**：父进程 `children` 表持有子进程 `Arc`，子进程 `parent` 字段只保存 `Weak`。
2. **group/session 非拥有索引**：`ProcessGroup::processes` 和 `Session::process_groups` 只保存 weak entry。
3. **init 进程存在性**：普通进程 reparent 依赖 `INIT_PROC` 已初始化。
4. **terminal 对象类型擦除**：`Session::terminal` 保存 `Arc<dyn Any + Send + Sync>`，只通过指针相等清除，不在 `kprocess` 内 downcast。
5. **zombie 回收顺序**：`free` 只能作用于 zombie 进程，避免 still-running 子进程从父表中被移除。

## 线程安全

| 类型 | `Send` 条件 | `Sync` 条件 |
|------|-------------|-------------|
| `Process` | 字段均满足 Send | `SpinNoIrq`、`AtomicBool` 和 `Arc` 保护共享状态 |
| `ProcessGroup` | 字段均满足 Send | `SpinNoIrq<WeakMap<...>>` 保护成员表 |
| `Session` | 字段均满足 Send | `SpinNoIrq` 保护进程组表和 terminal slot |
| `ThreadGroup` | 在 `SpinNoIrq` 内使用 | 不直接跨线程共享 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | PID、PGID 或 SID 冲突导致关系图错误 | 中 | 调用者用已存在 ID 创建进程、group 或 session | `create_session` 和 `create_group` 文档要求调用者先做冲突检查；`posix/process` 通过 registry 检查 group |
| T-02 | 跨 session 移动进程破坏 job-control 隔离 | 中 | 调用 `move_to_group` 时目标 group 属于其他 session | `move_to_group` 比较 `Arc<Session>`，不同 session 返回 `false` |
| T-03 | init 进程退出导致 reaper 缺失 | 中 | 调用者对 init 进程调用 `exit` | `Process::exit` 对 init 进程直接返回 |
| T-04 | 非 zombie 进程被提前回收 | 中 | 调用者对运行中进程调用 `free` | `free` 断言 `is_zombie`，错误调用触发 panic |
| T-05 | 控制终端被重复绑定 | 中 | 多个 TTY 尝试设置同一 session terminal | `set_terminal_with` 在 terminal 已存在时返回 `false` |
| T-06 | 错误终端对象清除当前绑定 | 中 | 调用者传入非当前 terminal 的对象调用 `unset_terminal` | `unset_terminal` 使用 `Arc::ptr_eq` 校验对象一致性 |
| T-07 | wait 或 procfs 遍历读到过期 group member | 低 | WeakMap 中存在已释放对象的 weak entry | `ProcessGroup::processes` 通过 `WeakMap::values` 返回可升级对象，registry 另有 cleanup 路径 |
| T-08 | 锁顺序误用导致死锁 | 中 | 外部持有 children、group 或 session 成员锁后调用 group/session mutation API | API 内部统一加锁；新增调用点应避免外层持有 `kprocess` 成员锁 |
| T-09 | group-exit 退出码被普通线程退出覆盖 | 中 | group exit 后其他线程继续调用 `exit_thread` | `exit_thread` 在 `group_exited` 为 true 时不覆盖 `exit_code` |
| T-10 | 中断上下文误用放大关中断区间 | 中 | 在中断上下文中执行进程关系 mutation，或持有 `SpinNoIrq` 后调用长路径逻辑 | `kprocess` API 内部锁区保持短小；新增调用点应限制在 task/syscall 生命周期路径 |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、权限或 job-control 语义错误。
- 低：导致统计不准、展示过期、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `init_proc` panic | `INIT_PROC` 尚未初始化 | 查询 init 进程失败 | fork/exit/reparent 路径不可用 | 2 | 启动入口先调用 `Process::new_init`；测试 helper 保证 init 存在 |
| F-02 | 子进程 reparent 失败 | init 进程未初始化或 children 锁顺序被外部破坏 | orphan child 留在退出父进程下 | wait 和 procfs 关系错误 | 2 | `Process::exit` 使用 `INIT_PROC` 作为统一 reaper，并在内部按固定顺序更新 children 和 parent |
| F-03 | wait 回收运行中进程 | 调用者绕过 zombie 检查调用 `free` | 父子关系提前删除 | wait、signal 和 procfs 观察错误 | 2 | `free` 对 `is_zombie` 做断言 |
| F-04 | setsid 或 setpgid 语义错误 | 调用者未检查 ID 冲突或 session 约束 | 进程组关系错误 | job-control 行为异常 | 3 | `move_to_group` 内部拒绝跨 session；冲突检查由 syscall 和 registry 执行 |
| F-05 | terminal slot 永久占用 | TTY drop 或 ioctl 路径未调用 `unset_terminal` | session 无法绑定新 terminal | TTY job-control 失效 | 3 | `set_terminal_with` 返回失败信号；`TIOCNOTTY` 路径调用 `unset_terminal` |
| F-06 | WeakMap 残留过期项 | process group 成员释放后索引未清理 | 遍历结果少于表项数量 | 统计或展示短暂不一致 | 4 | `WeakMap::values` 只返回可升级对象；`kthread` registry 提供 cleanup |
| F-07 | 线程集合统计不准 | 调用者漏调 `add_thread` 或 `exit_thread` | `threads()`、CPU time 和 rusage 统计错误 | procfs、wait、timer 逻辑受影响 | 3 | clone 和 exit 路径集中调用对应 API |
| F-08 | 中断上下文执行进程关系修改 | IRQ 路径误调用 `fork`、`exit`、`create_session` 或 group mutation | 关中断持锁时间变长 | 调度延迟上升，严重时影响系统响应 | 2 | 进程关系修改限定在启动、clone、exit、wait 和 syscall job-control 路径 |

严重度定义：

- 1：致命，系统崩溃、内存破坏。
- 2：严重，进程生命周期或 wait 语义不可用。
- 3：一般，job-control 或统计功能异常。
- 4：轻微，短暂展示不一致。

## 故障管理

- `move_to_group`、`set_terminal_with`、`unset_terminal` 使用 bool 返回调用是否成功。
- `create_session` 和 `create_group` 在当前进程已经是 leader 时返回 `None`。
- `init_proc` 在 init 尚未初始化时 panic，调用者需保证启动顺序。
- `free` 在目标不是 zombie 时 panic，调用者需先完成 wait 条件判断。
- 本 crate 不直接返回 Linux errno，errno 映射由 syscall 层完成。

## 隐私分析

`kprocess` 保存 PID、父子关系、线程 ID、退出码、进程组、session 和 terminal 绑定。
它不保存用户 payload、命令行、credential、文件路径或地址空间内容。
这些关系会被 procfs、wait、signal 和 job-control 路径读取，调用者需要在上层执行可见性和权限控制。

## 已知限制

- subreaper 尚未实现，普通退出进程的子进程统一 reparent 到 init。
- ID 冲突检查不在 `kprocess` 内集中执行，调用者需通过 registry 或 syscall 规则保证唯一性。
- `Session::terminal` 使用 `Any` 类型擦除，`kprocess` 只管理绑定槽，不了解具体 TTY 类型。
- `Process::exit` 不主动从 process group 成员表删除进程，成员表依赖 weak entry 释放和 cleanup。

## 审计清单

修改本模块时需验证：

- 新增公开 API 是否有外部调用者，内部 helper 优先保持 `pub(crate)`。
- 新增进程关系转换是否保持 parent/children、group/processes、session/process_groups 三组关系一致。
- 新增锁嵌套是否遵循现有 API 内部加锁方式，避免外部持有成员锁后调用 mutation API。
- 新增退出路径是否保持最后线程退出、zombie、wait/free 顺序。
- 新增 controlling terminal 行为是否保持 set-once 和 pointer-match unset 语义。
