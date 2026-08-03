# ktty - 设计文档

## 定位

`ktty` 实现 X-Kernel 的终端、行规程、伪终端和 POSIX 作业控制接口。
设备文件层把 `/dev/console`、`/dev/tty` 和 PTY 请求转发到本 crate，进程、
进程组与 session 身份由 `kprocess` 持有。

## 范围

- `src/terminal/job.rs`：控制终端关联的 session、前台进程组和前台变化通知。
- `src/terminal/ldisc.rs`：输入处理、规范模式和信号字符处理。
- `src/tty/mod.rs`：TTY 文件操作与 `TC*`/`TIOC*` ioctl 边界。
- `src/tty/ntty.rs`、`src/tty/pty.rs`：console TTY 与 PTY 后端。

## 控制终端状态

`TIOCSCTTY` 仅允许 session leader 获取控制终端。绑定成功时，`ktty` 同时：

1. 在 `kprocess::Session` 中安装控制终端对象；
2. 在 `JobControl` 中记录相同 session；
3. 将调用者所在进程组设为初始前台进程组。

这与 Linux 获取控制终端后的可观察行为一致，使随后执行的 shell 能通过
`TIOCGPGRP` 取得有效前台 PGID。任一步失败时，新安装的 session/terminal
状态会回滚。每个 TTY 的关联事务锁串行化绑定、解绑和回滚，防止旧事务撤销
同一 TTY 上已经成功的新绑定。

`TIOCSPGRP` 从用户空间读取目标 PGID，在进程表中解析目标进程组，并要求：

- 调用者属于该控制终端关联的 session；
- 目标进程组也属于同一 session。

`TIOCGPGRP` 和 `TIOCGSID` 仅向该控制终端所属 session 的调用者返回状态；跨
session 查询返回 `ENOTTY`。与 Linux 一致，PTY master 可以查询其配对 slave 的
作业控制状态。显式前台切换与初次控制终端绑定共享
`JobControl::set_foreground`，成功后唤醒等待前台状态变化的读操作。

TTY `open` 还实现 Linux 的隐式控制终端获取：未指定 `O_NOCTTY`、当前用户进程是
session leader 且尚无 controlling TTY 时，打开的终端会通过同一 `bind_to` 流程成为
控制终端并初始化 foreground。PTY master 不参与隐式或显式控制终端绑定，只有 slave
端可成为 controlling TTY。内核线程发起的 open 没有用户 session，因此不会触发该
状态转换；启动脚本的 fallback shell 会在用户态重新打开 `/dev/console`。

## 调用约束

TTY ioctl 路径要求当前任务是用户进程线程，因为权限与 session 校验依赖
`current_user_thread()`。用户指针只在 ioctl 边界读写，内部作业控制逻辑只接收
内核持有的 PGID、`Session` 和 `ProcessGroup`。该路径不可在中断上下文调用。
TTY open 可由内核线程调用；这种情况下只完成文件打开，不尝试分配 controlling TTY。

## 并发模型

控制终端的 session 和 foreground 分别由 `SpinNoIrq` 保护；TTY 级关联事务锁保护
跨 `Session`、`JobControl` 和 foreground 的多阶段绑定与解绑。状态更新不跨越可能
阻塞的操作；前台进程组变化后通过 `PollSet` 唤醒等待者。
