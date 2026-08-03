# ktty - 安全与可靠性分析

## 信任模型

用户空间可通过 TTY 文件描述符提交 ioctl 命令、用户指针和 PGID。`ktty` 不信任
这些 ABI 输入；进程、进程组、session 和控制终端身份以 `kprocess` 中的内核对象
为准。

## 外部边界与攻击面

- `TC*`/`TIOC*` ioctl 包含用户地址，必须通过 `osvm` 用户内存访问接口复制。
- `TIOCSPGRP` 的 PGID 可能无效、已退出或属于其他 session。
- `O_NOCTTY` 控制打开 TTY 时是否允许隐式取得控制终端。
- 任意打开的 TTY fd 不代表调用者拥有该终端的控制 session。
- console 输入和 PTY 对端输入属于外部数据，由行规程进行处理。

## 内存安全不变量

ioctl 用户指针只在 `src/tty/mod.rs` 的 ABI 边界解引用，并使用 `read_vm`/
`write_vm` 校验用户地址。作业控制层不保存用户指针，只保存 `Session` 和
`ProcessGroup` 的弱引用。

## 线程安全

termios、窗口大小、session 和 foreground 状态分别由自旋锁保护。锁内不执行
用户内存访问或进程表查找，避免扩大锁范围。foreground 更新完成后再唤醒 poll
等待者。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 跨 session 修改终端前台进程组 | 中 | 其他 session 持有可访问的 TTY fd | `set_foreground_for` 校验调用者 session 与终端 session 的对象身份 |
| T-02 | 将其他 session 的进程组设为 foreground | 中 | 用户提交有效但跨 session 的 PGID | `set_foreground` 拒绝 session 不一致的目标组并返回 `EPERM` |
| T-03 | 无效用户指针导致内核非法访问 | 高 | ioctl 参数指向未映射或不可访问地址 | 统一通过 `read_vm`/`write_vm` 访问并返回用户可见错误 |
| T-04 | `O_NOCTTY` 打开意外改变 session 状态 | 中 | 设备 open 忽略瞬态 open flag | `VfsFileBuilder::requests_no_controlling_tty` 在 flag 被清理前阻止隐式绑定 |
| T-05 | PTY master 被错误用作 controlling TTY | 高 | session leader 打开 `/dev/ptmx` | master 的隐式 open 和 `TIOCSCTTY` 绑定均返回而不改变 session |
| T-06 | 跨 session 查询 foreground 或 SID | 中 | 其他 session 继承或收到 TTY fd | `foreground_for`/`session_for` 校验调用者 session 对象身份 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 控制终端绑定后无 foreground | 绑定流程未初始化前台组 | shell 无法启用 job control | 交互式终端功能降级 | 3 | `bind_to` 在同一事务中安装 session、terminal 和 foreground |
| F-02 | 绑定过程留下部分状态 | foreground 设置失败或 bind/unbind 并发 | 后续 getty 无法重新获取终端 | console 登录不可用 | 2 | TTY 级关联事务锁串行化安装、回滚和解绑 |
| F-03 | 前台目标 PGID 不存在 | 进程组已退出或输入错误 | `TIOCSPGRP` 失败 | 当前 foreground 保持不变 | 4 | 设置前先通过 `kprocess::job_control::target_group` 解析目标 |
| F-04 | fallback shell 继承无 controlling TTY 的 stdio | stdio 由 PID-less 内核线程预先打开 | Bash 禁用 job control | 交互式 shell 功能降级 | 3 | fallback 路径在 PID 1 用户上下文重新打开 console，触发标准 TTY open 绑定 |

## 已知限制

后台进程组读取控制终端时，当前实现会等待其进入前台；尚未完整实现 Linux 的
`SIGTTIN`/孤儿进程组处理语义。

`TIOCSCTTY` 尚不支持 Linux 的特权 `arg == 1` 跨 session 强制夺取；该请求返回
`EPERM`，其他非零参数返回 `EINVAL`。

## 审计清单

- 新增 ioctl 是否在边界完成用户指针复制与数值校验。
- 控制终端、调用者和目标进程组是否属于同一 session。
- 多阶段状态更新失败时是否只回滚本次新安装的状态。
- foreground 变化后是否唤醒等待者且未持锁执行唤醒外的阻塞操作。
