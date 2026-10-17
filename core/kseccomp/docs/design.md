# kseccomp design

## 定位

`kseccomp` 是 seccomp(2) 的 classic-BPF 过滤器执行引擎与 `struct seccomp_data`
快照模型，是"能跑 dockerd 默认 seccomp profile"的必要组成部分。它只承担
cBPF 程序解释、程序校验、决策优先级与数据布局四件事，**不**持有线程/进程状态、
不读用户内存、不参与 syscall 分发。

相关 crate 的职责边界：

```text
ksyscall (sys_seccomp)  ->  逐字段解码用户 sock_fprog，调用 kseccomp 构造过滤器
                          ->  存到 kprocess::Thread.seccomp()
kseccomp                 ->  纯解释器 run(SockFilter[], SeccompData) -> u32
                          ->  SeccompFilter 校验（长度/指令编码/跳转范围）
                          ->  SeccompFilter::strict_allowlist() 生成 strict 白名单
kprocess::Thread         ->  持有 SeccompContext（mode + filter chain）
dispatch_irq_syscall     ->  syscall 入口先 evaluate_seccomp 再分发
```

## 架构

```text
用户态 sock_fprog
   │  (ksyscall 按字段解码 len/filter 指针，再逐条解码 sock_filter)
   ▼
SeccompFilter::new()  ── 校验空程序/BPF_MAXINSNS/指令编码/跳转范围
   │
   └─ SeccompFilter::strict_allowlist(&[nr])  ── strict 模式白名单
   ▼
Thread.seccomp()   (set_strict / append_filter / sync_from)
   ▼
syscall 入口构建 SeccompData{nr,arch,ip,args[6]}
   ▼
run(program, data.bytes)  ── cBPF 机器（A/X/M[0..16]），纯算术无 unsafe
   ▼
Action::from_ret ── highest_priority ── Decision{Allow,Errno,KillProcess,KillThread,UserNotif,Trace,Unsupported}
```

## 范围

- `SeccompData`：按 Linux `include/uapi/linux/seccomp.h` 布局的 64 字节快照，
  由 `SeccompData::new` 一个入口构造，保证 `nr/arch/ip/args` 偏移自洽，并提供
  x86_64/aarch64/riscv64/loongarch64 的 `AUDIT_ARCH_*` 常量。
- `run`：classic-BPF 解释器，支持 `LD`/`LDX` 的 IMM/ABS/IND/MEM/LEN、ST/STX、
  ALU（ADD/SUB/MUL/DIV/OR/AND/LSH/RSH/NEG/MOD/XOR）、JMP
  （JA/JEQ/JGT/JGE/JSET，源 K 或 X）、RET（K 或 A）、MISC（TAX/TXA）。
- `SeccompFilter::new`：空程序/`BPF_MAXINSNS` 上限，外加与 Linux
  `seccomp_check_filter` 对齐的指令编码与跳转范围校验。
- `Action::from_ret` / `highest_priority`：链式过滤器按 Linux
  `seccomp_run_filters` 的优先级合并；无法识别的 action 走最严优先级。
- strict 模式：`SeccompFilter::strict_allowlist()` 用 `linux_sysno::Sysno`
  传入架构正确的 syscall 号，生成 read/write/_exit/rt_sigreturn 白名单程序，
  复用同一 `evaluate` 引擎。
- `opcode` 模块集中 classic-BPF 编码常量与位域提取函数，供解释器与校验器共用。

## 设计决策

- **解释器纯函数化**：所有不合法指令、越界 load、除零都以
  `SECCOMP_RET_KILL_THREAD` 兜底，绝不"宽容放行"，对齐 Linux"非法过滤器不能
  静默放行 syscall"的行为。
- **非法程序在安装期拒绝**：`SeccompFilter::new` 校验指令编码与所有前向跳转
  目标，非法程序返回 `SeccompFilterError::InvalidInstruction`，由 syscall 层
  映射为 `EINVAL`。把错误暴露在安装点，而不是让它在首个 syscall 上杀掉任务。
- **RET 保留全 32 位**：action 与 errno/data 低 16 位一起返回，决策解码（
  `Action::from_ret`）时才用 `SECCOMP_RET_ACTION_FULL`/`DATA` 拆分。
- **KILL_PROCESS 用全宽比较**：0x80000000 超出 action 掩码，`from_ret` 按
  `SECCOMP_RET_ACTION_FULL` 一次匹配全部 action；`TRAP` 与未知取值都归为
  `Action::Unknown`，其 `precedence()` 最小（最严），因此不会被忽略。
- **strict 白名单按 arch 生成**：避免硬编码 x86 号；调用方传入
  `Sysno::read/write/exit/rt_sigreturn` 的号，aarch64/riscv64 与 x86_64 都正确。
- **评估不分配**：`SeccompContext::evaluate` 用单次 fold 求最小优先级，
  在持有 `SpinNoIrq` 的 syscall 热路径上不分配内存。

## 调用约束 / 执行上下文

- `run`/`SeccompFilter::new`/`highest_priority` 为纯计算，可在任意上下文调用；
  本 crate `#![forbid(unsafe_code)]`。
- `SeccompContext` 用 `SpinNoIrq` 保护 mode 与 filter 链：syscall 入口每次
  评估都加锁遍历，链长即过滤器个数，属 syscall 热路径；整条链的长度受 Linux
  `MAX_INSNS_PER_PATH` 约束（见 `kprocess`），评估路径不分配。
- 安装过滤器只在 `sys_seccomp` 中发生，不在中断上下文；`inherit_from`/
  `is_syncable_with`/`sync_from` 先取快照再落锁，任何路径都不会同时持有两个
  context 的锁。

## 边界与非目标

- 不实现 `SECCOMP_RET_USER_NOTIF` / `SECCOMP_GET_NOTIF_SIZES` /
  `SECCOMP_RET_TRACE`（枚举为 `UserNotif`/`Trace`，syscall 层映射为
  `ENOSYS`，fail closed）。
- `TRAP` 无法交付用户回调，因此与未知 action 一样按最严优先级处理，
  由 dispatch 以 `SIGSYS` 终止线程（fail closed）。
- 不做 cBPF→内核 eBPF 翻译或 eBPF verifier；逐指令解释 + 安装期校验即可满足
  容器默认 profile 与安全边界。