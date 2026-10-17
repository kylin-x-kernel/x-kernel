# kseccomp security and reliability

## Scope, assets, and boundaries

seccomp 是客户机内用户代码对内核施加的自愿约束沙箱：**信任边界在内核与
用户代码之间**，过滤器的目标是减少恶意/受损用户进程可使用的 syscall 面。
本 crate 的解释器位于该边界的内核侧，因此必须满足：

1. 永不放大权限：任何解释异常（未知指令、越界、除零）都必须 fail closed，
   而不是回到"未过滤"的放行。
2. 永不泄露内核数据：`run` 只能读取它拿到的那份 `SeccompData` 快照，
   无其他内存读写（本 crate 不含 `unsafe`）。
3. 拒绝路径必须交付给 syscall 层可执行的确定性结果，且决策优先级与 Linux
   一致（否则恶意程序可借"两条过滤器返不同 action"绕过预期约束）。

## 不变量与承诺

- **fail-closed 兜底**：`run` 默认返回 `SECCOMP_RET_KILL_THREAD`；越界
  load 返回 0、除零/非法指令/非法跳转立即以 deny 终止，绝不越过程序边界
  或访问快照之外内存。
- **无法识别的 action 不放行**：`TRAP` 与任何未识别返回值归为
  `Action::Unknown`，其优先级最小，`highest_priority` 得到
  `Decision::Unsupported`，由 dispatch 以 `SIGSYS` 终止线程。
- **确定性**：程序是无状态的直线机器（A/X/M + pc），同程序同快照必得同结果；
  无熵、无并发共享、无全局状态。
- **数据最小化**：`SeccompData` 快照只含 seccomp ABI 需要的字段的 64 字节；
  解释器不触及其余内核内存。
- **长度上限**：`SeccompFilter::new` 强制 `BPF_MAXINSNS=4096`，防止畸形
  用户程序拖垮内核或引发超额分配（ksyscall 侧在分配前先校验）。
- **安装期校验**：指令编码与所有前向跳转目标在安装前校验，非法程序返回
  `EINVAL`；不会出现"装得进去、跑起来必杀"的模糊失败。
- **配合 PR_SET_NO_NEW_PRIVS / CAP_SYS_ADMIN**：seccomp 安装门在
  `sys_seccomp` 侧（特权或 no_new_privileges），本 crate 只保证已安装的
  过滤器被忠实地解释，不自行做授权。

## 对沙箱逃逸的防御

| 攻击方向 | 防御 |
| --- | --- |
| 过滤器返回 ALLOW 绕过 | 链式合并取最高优先级；ALLOW 在 `Action::precedence` 中优先级最低 |
| 用 TRAP / 未知返回码绕过 | `Action::Unknown` 优先级最高（最严）→ `Decision::Unsupported` → dispatch `do_exit(SIGSYS)` |
| 用 TRACE/USER_NOTIF 绕过 | 枚举为不可执行 → `GET_ACTION_AVAIL` 返回 `ENOSYS`，dispatch 对 syscall 返回 `ENOSYS`，不会放行 |
| 越界 `BPF_ABS/IND` 读取内核内存 | load 全部以 64 字节快照边界校验，越界补零 |
| 除零 / 移位崩溃 | ALU 除零/取模零在安装期即 `EINVAL`，运行期仍以 deny 兜底；移位按 `wrapping_*` 语义 |
| 超大 `sock_fprog.len` 或超长链拖垮内核 | `SeccompFilter::new` 与 ksyscall 双重 `BPF_MAXINSNS`；链总长受 Linux `MAX_INSNS_PER_PATH` 约束 |
| 用非法程序把内核变成"必然杀进程"的放大器 | 安装期校验先行拒绝，调用方可在安装点看到 `EINVAL` |
| syscall 入口不作为（漏评估） | `dispatch_irq_syscall` 在分发前统一 `evaluate_seccomp`，单点入口 |
| strict 白名单生成错（放行错误 syscall） | `SeccompFilter::strict_allowlist` 由单元测试固定"只放行四个号、其余 kill"的语义；号来源是 `linux_sysno::Sysno` |

## 审计要点

- `bpf::run`：非法指令/越界/除零的全部分支都归结到 `DEFAULT_RETURN`
  （deny），不应出现可放行的隐式路径。
- `filter::validate`：类/尺寸/模式/运算符与前向跳转目标全部校验；任何新增
  可执行编码都要同步扩这里。
- `Action::from_ret` / `highest_priority`：Unknown > KILL_PROCESS >
  KILL_THREAD > ERRNO > USER_NOTIF > TRACE > LOG > ALLOW 的优先级表与
  `seccomp_run_filters`（含"未知取值最严"策略）一致。
- `SeccompFilter::strict_allowlist`：布局必须保证每条 `JEQ` 的 `jt` 正好跳到
  末尾的 `ALLOW`，且不破坏累加器（不能用 load 指令充当跳板）。
- ksyscall 的安装门：`flags` 白名单（TSYNC/TSYNC_ESRCH/LOG/SPEC_ALLOW），
  未知 flag 一律 `EINVAL`，避免静默忽略。

## 已知边界

- `USER_NOTIF`/`TRACE` 未实现：`GET_ACTION_AVAIL` 对二者返回 `ENOSYS`，
  dispatch 对二者 fail closed → `ENOSYS`。不会静默放行。
- `TRAP` 不交付 `SIGSYS` 回调帧（Linux 会用 `SECCOMP_RET_TRAP` 的 data 构造
  `siginfo`），当前按最严处理直接终止线程；进程若依赖 `SIGSYS` handler 会
  观察到差异。
- `SECCOMP_FILTER_FLAG_LOG` 被接受但未产生审计日志；`SPEC_ALLOW` 无副作用。
- TSYNC 只广播到"已发布"的兄弟线程；正在构造、尚未发布的线程将在发布后
  从仍在执行的线程继承（继承点见 `Thread::prepare_*_fork/thread_clone`）。