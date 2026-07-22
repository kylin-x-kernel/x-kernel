# posix-process — 设计文档

## 定位

`posix-process` 负责进程/线程生命周期相关的上层 owner 逻辑：

- clone / exit / signal-return 需要共享的用户态 trap 主循环；
- 初始用户进程的地址空间、`Process`/`Thread` runtime、TTY 和 stdio 组装；
- 线程退出时的 robust futex 清理、group-exit 和父进程通知；
- 保持这些逻辑依赖 `kprocess` 原语，但不把它们塞回 `kprocess` 本体。

纯 syscall adapter（`getpid`、`getrusage`、`umask`、job control、rlimit 等）
已经迁回 `ksyscall/task`，不再由本 crate 承接。

## 范围

本次相关范围包括：

- `src/runtime.rs`
- `src/init_process.rs`
- `src/lib.rs`

## 架构

```text
entry / ksyscall
        |
        v
  posix-process
    |   \
    v    v
  kexec  kprocess
```

## 调用约束 / 执行上下文

- `new_user_task()` 仅用于创建会进入用户态执行的 task，并且要求调用方先准备好该线程的 `PidHandle` 和 `Thread`。
- 用户 task 启动路径必须遵守：
  - 先由 process-domain owner 决定 PID namespace 和线程 identity
  - 先构造 matching `Thread`，再通过 `new_user_task(..., thread, ...)` 一次性构造 task 与 `UserTaskRuntime`
  - 调用 `start_user_task(...)`
  - `kprocess` 内部先完成 publish，再使 task runnable
- `spawn_init_process()` 依赖 rootfs、TTY 和默认 stdio 初始化路径可用；它由 PID-less 的 late-init 线程调用，分配 PID 1 并构造一个全新的 `User` 身份用户任务（走与 fork 相同的 `new_user` + `publish_user_task().commit()` 路径），不再原地转换 current task。
- `do_exit()`、`check_signals()` 依赖 current task 是携带 `UserTaskRuntime` 的用户 task。
- 这些接口会访问地址空间、信号状态、fd 表和共享内存管理器，可阻塞，不适用于中断上下文。

## 状态机

### 用户线程运行

1. task 完成发布并进入 run queue。
2. 进入用户态运行 `UserContext`。
3. 因 syscall / page fault / exception / interrupt 返回内核。
4. 处理返回原因并更新 CPU 计时状态。
5. 执行信号检查和默认动作。
6. 轮询 CPU timer 后返回用户态。

### 线程退出

1. 清理 `clear_child_tid` 并唤醒 futex。
2. 遍历 robust futex list，标记 owner-dead。
3. 从进程线程集合中摘除当前线程。
4. 若为最后线程，关闭 fd、通知父进程、清理共享内存和可选 TEE 私有状态。
5. 若触发 group exit，向线程组广播 `SIGKILL`。

## 并发模型

- 线程/进程基础状态由 `kprocess` 和其内部锁保护。
- 本 crate 负责组织退出与信号路径的调用顺序，不重复持有额外全局状态。
- robust futex owner-dead 标志通过原子位和等待队列协作。

## 设计决策

- 该逻辑放在 `posix-process`，因为它围绕进程/线程生命周期状态机，不应该污染 `kprocess` 的基础职责。
- `posix-process` 可以自然承接这类面向进程生命周期的上层 owner 逻辑，并避免 `kprocess <-> posix-ipc` 环依赖。
- 纯 adapter 迁回 `ksyscall/task` 后，本 crate 只保留真正依赖进程生命周期状态机的 owner 逻辑。
- 用户态 runtime 直接消费 `MmSpace::handle_page_fault()` 的结构化结果，
  因此架构 trap glue 不需要理解 file-backed fault 细节，同时 runtime 可以把
  file-backed EOF 等对象级 fault 转换为 `SIGBUS`，把普通权限或缺页错误转换为
  `SIGSEGV`。
