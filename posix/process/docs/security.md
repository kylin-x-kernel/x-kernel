# posix-process — 安全与可靠性分析

## 信任模型

- 用户态 trap 原因、robust futex 指针、`clear_child_tid` 地址都不可信。
- 初始用户进程可执行路径、argv/envp 内容和 rootfs 解析结果都来自外部输入。
- 本 crate 负责在当前线程/进程语义内消费这些输入，并把失败限制在当前进程生命周期内。

## 外部边界 / 攻击面

- clone/exit/signalfd/signal-return 相关 syscall 路径。
- init 进程启动时的可执行文件解析、TTY 绑定和 stdio 安装路径。
- 线程退出时读取的 robust futex list。
- 共享内存回收和父子进程退出通知。

## unsafe 代码清单

- `uctx.emulate_unaligned()`：
  依赖目标架构 `UserContext` 实现的安全前提。
- `&raw const (*head).list` 和后续 `read_vm()` 遍历：
  依赖调用方提供的是当前线程登记过的 robust list 头指针，且读取失败时立即停止。

## 内存安全不变量

- robust futex 地址必须能转换成当前进程 futex key。
- 初始用户进程的 `Thread` 必须在 `spawn_task` 前完整安装 task extension。
- 最后线程退出前必须先关闭 fd，再标记进程退出，避免外部等待者持有悬挂资源语义。
- `SHM_MANAGER` 清理仅针对已退出进程 PID。

## 线程安全

- 本 crate 不自建额外共享状态，依赖 `kthread`/`ProcessState` 内部同步。
- group-exit 广播和父进程唤醒都基于当前可见线程/进程集合执行。

## 威胁分析

- 恶意 robust list 构造循环：通过 `ROBUST_LIST_LIMIT` 限界。
- 无效用户地址：`read_vm()` 失败即停止遍历。
- `rt_sigreturn` 后重复进入信号处理：`SkipSignalCheckOnce` 显式规避。
- init 可执行文件解析失败：当前实现直接 panic，保留“系统无法启动即失败停止”的语义。

## 故障模式与影响分析（FMEA）

- 退出路径漏关 fd：会破坏 pipe EOF / wait 语义；当前实现先 `close_all_fds()`。
- 父进程通知丢失：通过退出信号和 `child_exit_event()` 双路径通知。
- group-exit 未广播：会留下残余线程；当前实现遍历线程组发 `SIGKILL`。
- init 进程启动前 task extension 未安装：会导致用户线程 runtime 前提失效；当前实现先安装 `KTaskExt` 再 `spawn_task()`。

## 故障管理

- 用户输入错误优先提前返回或终止当前进程。
- 线程/进程内部不变量破坏时，允许升级为 fatal signal 路径。

## 已知限制

- `Stop` / `CoreDump` 默认动作仍是简化实现。
- 多线程 `execve` 仍未完整支持。
