# kcpu — 设计文档

## 定位

`kcpu` 为 x-kernel 提供多架构 CPU 特权指令与结构抽象。它是内核硬件抽象层的
核心组件，负责 trap/异常处理、任务上下文切换、用户态进入/退出以及用户空间内存
安全拷贝。调度器、信号处理、系统调用分发和驱动中断处理均依赖本 crate。

## 背景

x-kernel 需要在多种 CPU 架构上运行，每种架构的寄存器布局、异常入口机制、
上下文切换方式和特权指令各不相同。本 crate 将这些差异封装在统一的 API 后面，
使上层内核代码无需关心具体架构细节。

## 范围

涉及的源文件：

```text
arch/kcpu/
├── src/
│   ├── lib.rs                        # cfg_if 架构分发、共享宏
│   ├── excp.rs                       # 分布式 trap handler 注册
│   ├── active_exception_context.rs   # per-CPU 当前 trapframe 跟踪
│   ├── userspace_common.rs           # ReturnReason、异常表修正
│   ├── x86_64/
│   │   ├── mod.rs                    # 导出 ExceptionContext(=TrapFrame)、TaskContext、单元测试
│   │   ├── ctx.rs                    # ExceptionContext（含 orig_rax）、ContextSwitchFrame、TaskContext
│   │   ├── excp.rs / excp.S          # trap 入口、IRQ/PageFault 分发、syscall_entry
│   │   ├── instrs.rs / copy_user.S   # user_copy + 异常表条目
│   │   ├── userspace.rs              # UserContext、EnterUserFrame、UserRestorableContext、enter_user、syscall MSR 初始化、syscall_restart_error
│   │   ├── boot.rs                   # init_trap()
│   │   ├── gdt.rs                    # GDT/TSS per-CPU 初始化
│   │   └── idt.rs                    # IDT 初始化
│   ├── aarch64/
│   │   ├── mod.rs                    # 导出、init_trap()、enable_user_timer_access()
│   │   ├── ctx.rs                    # ExceptionContext、FpState、TaskContext
│   │   ├── excp.rs / excp.S          # 异常向量表、ESR 分发
│   │   ├── instrs.rs / copy_user.S   # raw_copy_from_user
│   │   └── userspace.rs              # UserContext、enter_user
│   ├── riscv/
│   │   ├── mod.rs                    # 导出、GeneralRegisters
│   │   ├── ctx.rs                    # GeneralRegisters、FpState、ExceptionContext、TaskContext
│   │   ├── excp.rs / excp.S          # scause 分发
│   │   ├── instrs.rs / copy_user.S   # user_copy
│   │   ├── userspace.rs              # UserContext、enter_user
│   │   ├── boot.rs                   # init_trap()
│   │   ├── hwprobe.rs                # riscv_hwprobe 能力快照、聚合与 WHICH_CPUS 匹配 helper
│   │   └── macros.rs                 # 汇编宏（LDR/STR、FP、异常表）
│   └── loongarch64/
│       ├── mod.rs                    # 导出、UnalignedError
│       ├── ctx.rs                    # GeneralRegisters、FpuState、ExceptionContext、TaskContext
│       ├── excp.rs / excp.S          # estat 分发
│       ├── instrs.rs / copy_user.S   # user_copy
│       ├── userspace.rs              # UserContext、enter_user
│       ├── boot.rs                   # init_trap()
│       ├── macros.rs                 # 汇编宏（STD/LDD、FP、异常表）
│       └── unaligned.rs / unaligned.S # 非对齐访问指令模拟
├── Cargo.toml
└── README.md
```

## 架构

### 分层结构

```text
                  ┌───────────────────────┐
                  │  上层内核              │
                  │  (调度器/信号/syscall) │
                  └───────────┬───────────┘
                              │
              ┌───────────────┼───────────────┐
              │   kcpu 统一 API                │
              │   ExceptionContext / TrapFrame  │
              │   UserContext / TaskContext     │
              │   init_trap() / user_copy()     │
              └───────┬───────────────────────┘
                      │ cfg_if 分发
        ┌─────────────┼─────────────────┐
        │         │         │         │
     x86_64    aarch64    riscv    loongarch64
```

### 共享层与架构层

| 层次 | 组件 | 职责 |
| ---- | ---- | ---- |
| 共享 | `excp.rs` | `IRQ`、`PAGE_FAULT` 分布式切片，`dispatch_irq_trap!` 宏 |
| 共享 | `active_exception_context.rs` | per-CPU 当前 trapframe 跟踪（`ExceptionContextGuard`） |
| 共享 | `userspace_common.rs` | `ReturnReason`、`ExceptionKind`、异常表排序与修正 |
| 架构 | `ctx.rs` | `ExceptionContext`、`TaskContext`、FP 状态 |
| 架构 | `excp.rs` + `excp.S` | 汇编 trap 入口 → Rust handler |
| 架构 | `instrs.rs` + `copy_user.S` | 用户空间安全拷贝 + 异常表条目 |
| 架构 | `userspace.rs` | `UserContext`、`enter_user` 汇编调用 |
| 架构 | `boot.rs` / `mod.rs` | `init_trap()` 初始化 |

### 类型关系

```text
ExceptionContext  (每架构定义，完整寄存器 trap frame)
      │
      ├── 别名: TrapFrame = ExceptionContext  (所有 mod.rs)
      │
      ├── x86_64 额外字段:
      │     orig_rax: syscall 入口保存的原始系统调用号
      │              非 syscall 陷阱设为 u64::MAX（全 1）
      │     is_from_syscall() / orig_sysno() 查询入口信息
      │     rollback_syscall() 回滚 rip 以便重新执行 syscall
      │     restart_with_syscall(sysno) 以新系统调用号重启
      │
      └── 被 trap handler 和 UserContext 使用

UserContext       (每架构定义，包装 ExceptionContext + 用户态专用字段)
      │           x86_64:      { tf: TrapFrame, fs_base, gs_base }
      │                          - syscall_restart_error() 查询重启错误码
      │           aarch64:     { tf: ExceptionContext, sp, tpidr, saved_syscall_arg0 }
      │           riscv:       UserContext(ExceptionContext)     -- newtype
      │           loongarch64: UserContext(ExceptionContext)     -- newtype
      │
      └── Deref/DerefMut → ExceptionContext
          (通过 impl_user_context_deref! 宏)

EnterUserFrame    (x86_64 私有 trap-return 汇编帧)
      │           { kernel_rsp, tf: TrapFrame }
      │           - kernel_rsp 为 enter_user 返回 Rust 调用者所需的内核栈指针
      │           - tf 作为临时 trap stack，TSS.rsp0 指向其末尾
      │           - 不进入 UserContext，也不进入用户可见 signal frame

UserRestorableContext
      │           信号帧保存的窄类型，仅包含 ABI `mcontext_t` 未覆盖且允许
      │           从用户信号帧恢复的用户态语义状态；不包含 x86_64
      │           EnterUserFrame 私有字段或 syscall restart scratch 字段。

TaskContext       (每架构定义，callee-saved 寄存器用于上下文切换)
                  x86_64:      { kstack_top, rsp, fs_base, cr3, ext_state? }
                  aarch64:     { sp, r19-r29, lr, tpidr_el0, ttbr0_el1, fp_state? }
                  riscv:       { ra, sp, s0-s11, tp, satp, fp_state? }
                  loongarch64: { ra, sp, s[10], tp, pgdl, fpu? }

                  (带 ? 字段受 fp-simd feature 门控)
```

## 调用约束 / 执行上下文

- **`init_trap()`**：必须在 per-CPU 数据初始化之后、调度器启动之前调用。
  每个架构在该函数中完成异常表排序、硬件描述符表加载、trap 向量基址设置。
- **trap handler**（如 `x86_trap_handler`）：运行在中断关闭上下文中，
  不可睡眠或阻塞。由汇编入口直接调用，调用时栈上已保存完整 trap frame。
- **riscv64 `gp` 不变量**：`gp`（x3）是每 CPU 的 percpu 基址，启动时由
  `init_percpu_reg` 设置一次，S-mode trap 期间恒定，故**不**由
  `PUSH_POP_GENERAL_REGS` 保存/恢复——绝不放入可迁移的每任务 trapframe。
  U-mode 用户 `gp` 经 slot-3（`UserContext.regs.gp`）中转：`.Lexit_user`
  入口保存，`.Ltrap_return` 仅在返回 U-mode（SPP==0）时恢复；S-mode 返回
  保持本 hart 的 `gp`。原因：handler 若在 trap 途中阻塞（如 page-fault
  后端），任务会在 PUSH 与 POP 之间迁移到另一 hart，此时从 trapframe 恢复
  旧 hart 的基址会污染 `current()`/`this_cpu_id()`。
- **riscv64 hwprobe snapshot**：`init_hwprobe_from_fdt()` 在平台早期初始化
  从 enabled CPU device-tree node 采集 `riscv,isa` /
  `riscv,isa-extensions`、vendor/arch/impl id、`timebase-frequency`、
  Zicboz block size 和当前用户虚拟地址上限。
  `RiscvHwprobe` private provider 持有 per-CPU capability snapshot；
  对外 helper 接收 Linux raw key，在 `kcpu` 内部完成取值、聚合和
  `WHICH_CPUS` 匹配语义。syscall 路径不读取 M-mode CSR，也不重新解析设备树。
- **`TaskContext::switch_to()`**：必须在中断关闭上下文调用。
  涉及页表切换、TLS 更新和 FP 状态保存/恢复。
- **`UserContext::run()`**：禁用本地 IRQ 后进入用户态，返回后重新启用。
  调用者必须确保当前处于进程线程上下文且用户页表已正确设置。
- **`user_copy()`**：可在任意内核上下文调用。依赖异常表机制安全处理无效
  用户指针，不会因用户空间地址错误而 panic。
- **`active_exception_context()`**：可在任意上下文调用（包括 NMI 类上下文），
  返回 best-effort 快照。
- **FP/SIMD 保存恢复**：受 `fp-simd` feature 控制。未启用时跳过 FP 状态操作。
- **TLS 保存恢复**：受 `tls` feature 控制。未启用时跳过线程指针操作。

## 算法流程

### Trap 处理流程

```text
1. CPU 触发 trap/异常/IRQ
2. 汇编入口 (excp.S):
   - 保存所有通用寄存器到内核栈
   - 在栈上构建 ExceptionContext 结构
   - 调用架构对应的 Rust handler:
     x86_64:      x86_trap_handler(&mut ExceptionContext)
     aarch64:     dispatch_exception(&mut ExceptionContext, kind, source)
     riscv:       riscv_trap_handler(&mut ExceptionContext)
     loongarch64: loongarch64_trap_handler(&mut ExceptionContext)
3. Rust handler:
   a. 创建 ExceptionContextGuard（安装 per-CPU 活跃 trapframe 指针）
   b. 按向量号/cause/ESR/estat 分类异常
   c. Page Fault: 读故障地址 → 尝试注册 handler → 尝试异常表修正 → panic
   d. IRQ: dispatch_irq_trap!(IRQ, ...)
   e. Breakpoint: 日志 + 推进 PC
   f. LoongArch64 非对齐: emulate_unaligned() 指令模拟
   g. 其他异常: panic 附加诊断信息
4. 汇编出口:
   - 从 ExceptionContext 恢复寄存器
   - 从 trap 返回 (iretq/eret/sret/ertn)
```

#### AArch64 NMI 入口（pseudo / hardware）

- **pseudo-NMI**：`dispatch_exception` 创建 `NmiExceptionGuard`（仅 `nmi-pseudo`
  feature 且 `karch::pmr::is_ready()` 时激活）：把入口 `ICC_PMR_EL1` 存入
  trapframe 的 `pmr` 字段并打开 PMR mask；`use_nmi_path()` 以 `pmr <= NMI_ONLY`
  把中断分类为 NMI；返回前先 `daifset` 屏蔽 IRQ，再恢复入口 PMR。
- **hardware NMI**：异常入口（SPINTMASK=0）由硬件置 `PSTATE.ALLINT=1`，异常处理
  无需额外入口代码；`use_nmi_path()` 以 `karch::allint_active() && SPSR.I == 1`
  分类（IRQ 屏蔽时仍到达的中断必然是 Superpriority NMI）；ALLINT 窗口由 GIC IRQ
  dispatch 路径打开，`ERET` 从 SPSR 恢复中断前状态。
- **运行时降级**：上述入口处理均以机制 readiness 为门控（`pmr::is_ready()` /
  `allint_active()`），机制不可用（GICv2、无 FEAT_NMI）时行为与普通 IRQ 内核完全
  一致，从不触碰 NMI-only 寄存器。

### 用户态进入/退出流程

```text
UserContext::run(&mut self) -> ReturnReason:
1. karch::disable_local_irq()
2. 保存内核 TLS 指针，设置用户 TLS 指针
3. 调用 enter_user() [汇编]:
   - 从 trap frame 恢复用户寄存器
   - 切换用户页表
   - 返回用户态 (sysret/eret/sret/ertn)
4. [用户态执行，直到 trap/异常/syscall]
5. 汇编 trap 入口触发，保存寄存器，返回 run()
6. 恢复内核 TLS 指针
7. 分类返回原因:
   - Syscall: 特定异常号 (int 0x80 / SVC / ecall / syscall)
     x86_64: syscall_entry 汇编将 rax 压入 orig_rax 字段，
     非 syscall 陷阱压入 -1（u64::MAX）作为哨兵值
   - Page Fault: 从架构寄存器读故障地址
   - Interrupt: 分发 IRQ
   - 其他: 包装为 ExceptionInfo
8. karch::enable_local_irq()
9. 返回 ReturnReason

#### x86_64 syscall 重启机制

syscall 可被信号中断并需要重启。`orig_rax` 字段保存入口时的系统调用号：

- `is_from_syscall()`：检查 `orig_rax != u64::MAX`，区分 syscall 和普通陷阱
- `rollback_syscall()`：恢复 `rax = orig_rax`，`rip -= 2`（syscall 指令长度）
- `restart_with_syscall(sysno)`：`rax = sysno`，`rip -= 2`（用于 restart_syscall）
- `syscall_restart_error()`：检查返回值是否为 ERESTARTSYS/ERESTARTNOINTR/
  ERESTARTNOHAND/ERESTART_RESTARTBLOCK，返回对应的 LinuxError

信号处理代码在 delivery 前调用 `syscall_restart_error()` 判断是否需要重启，
然后通过 `rollback_syscall()` 或 `restart_with_syscall()` 调整用户态寄存器。
```

### 异常表修正机制

内核异常表（`__ex_table` 链接段）用于安全处理内核态访问用户空间内存时的
缺页异常：

1. `copy_user.S` 汇编中通过 `_asm_extable` 宏为每条可能故障的指令生成
   `{from, to}` 映射条目。
2. 启动时 `init_exception_table()` 按起始地址排序条目。
3. 发生内核态 Page Fault 时，`fixup_exception()` 对 `self.ip()` 进行
   二分查找。若命中，将 IP 设为恢复地址（`to`），返回 `true`。
4. 效果：跳过故障指令，将错误返回给 `user_copy()` 的调用者，
   内核不会因无效用户指针而 panic。

## 并发模型

- **per-CPU 数据**：`ACTIVE_EXCEPTION_CONTEXT_PTR`、x86_64 的 `TSS` 和 `GDT`
  均通过 `#[percpu::def_percpu]` 定义，每个 CPU 有独立副本，无需锁保护。
- **`ExceptionContextGuard`**：安装 trapframe 时保存原始 per-CPU slot 地址，
  并用 `AtomicUsize::swap` 记录前一个值。Drop 时不重新计算当前 CPU，而是对
  原始 slot 和当前 CPU slot 做 `compare_exchange(ptr, prev)` 恢复。这样即使
  page fault 后端或其它 trap handler 路径阻塞并迁移，迟到的 guard drop 也不会
  把恢复操作写到错误 CPU；若 slot 已经被调度器清空或新的 trap 覆盖，CAS 失败并
  保留新状态。迁移后内层 guard 释放时必须把当前 CPU slot 恢复为 `prev`，因为
  `prev` 是同一任务嵌套 guard 链里的外层 live `ExceptionContext`；将其清零会让
  外层 trap handler 后半段错误通过 `in_exception_context()` 检查并允许抢占。
- **exception context 挂起/恢复**：调度器切离一个仍在 trap handler 内的任务时，
  会把当前 CPU 的 active-exception 指针挂起并清零；当该任务的上下文切回并从
  `switch_to()` 返回时，再把挂起的指针恢复到当前 CPU。这样既不会在旧 CPU 留下
  stale 标记，也能保证任务迁移后继续执行 trap handler 后半段时
  `in_exception_context()` 仍为真。
  `Ordering::Relaxed` 足够，因为该指针只用于 best-effort 诊断和抢占门控，
  不发布跨 CPU 数据。
- **分布式切片**：`IRQ` 和 `PAGE_FAULT` 使用 `linkme` 分布式切片，
  编译期注册，运行时不可变，无需同步。
- **IRQ 关闭保护**：`switch_to()` 和 `run()` 在操作上下文前关闭本地 IRQ，
  防止上下文切换过程中被中断打断。
- **naked 函数**：所有 `context_switch`、FP 保存/恢复和 LoongArch64 非对齐
  辅助函数使用 `#[unsafe(naked)]` + `naked_asm!`，完全控制函数序言/尾声。

## 设计决策

### 为什么用 `cfg_if` 而非 trait object

每种架构在编译时确定，不存在运行时切换需求。`cfg_if` 实现零开销分发，
无虚函数表或间接调用开销。

### 为什么 `UserContext` 通过 `Deref` 而非继承暴露 `ExceptionContext`

Rust 不支持继承。`impl_user_context_deref!` 宏为 `UserContext` 实现
`Deref<Target = ExceptionContext>`，使调用者可直接在 `UserContext` 上
调用 `ExceptionContext` 的方法，同时保持两个类型的独立演化。

### 为什么每架构的 `UserContext` 布局不同

每种架构的用户态切换需要不同的附加状态：

- x86_64 需要独立的 `fs_base` 和 `gs_base`（MSR 切换）。
- aarch64 需要独立的 `sp`（EL0 栈指针不在 trap frame 中）和 `tpidr`。
- riscv 和 loongarch64 的 `ExceptionContext` 已包含完整状态，
  `UserContext` 使用 newtype 包装即可。

x86_64 的 `UserContext::run()` 会临时构造 `EnterUserFrame` 并传给汇编
`enter_user`。这样 `UserContext` 保持为用户寄存器语义状态，而 TSS.rsp0、
kernel stack restore slot、trap frame stack alignment 等汇编私有不变量集中在
`EnterUserFrame` 内。

信号处理不直接把完整 `UserContext` 放入用户栈。`UContext/MContext` 承载
用户 ABI 寄存器状态，`UserRestorableContext` 只补充 ABI 上下文未覆盖的窄状态，
避免 x86_64 `EnterUserFrame.kernel_rsp` 等内核私有字段泄露到 signal frame 或被
`sigreturn` 恢复。

### 为什么 LoongArch64 有软件非对齐模拟

LoongArch64 架构对非对齐访问触发异常而非由硬件处理。内核需要模拟这些访问
以保证正确性。`emulate_unaligned()` 解码故障指令的操作码，确定操作类型
（加载/存储、大小、有符号/无符号、通用/浮点寄存器），执行相应操作后推进 PC。

### 为什么 exec 成功后要重置用户通用寄存器

`execve` 成功装载新镜像后，入口用户寄存器必须符合目标架构 ELF entry ABI，
不能继承旧程序 syscall trap 时的寄存器值。各架构 `UserContext::reset_for_exec()`
负责清零所有不应继承的通用寄存器以及 syscall restart 相关状态（`orig_rax`、
`saved_syscall_arg0`、`from_syscall`），由调用方随后重建新的 IP/SP/TLS。

- x86_64 的 `execve(path, argv, envp)` 第三个参数 `envp` 位于 `rdx`；静态
  glibc 的 `_start` 把入口 `rdx` 当作 `rtld_fini` 回调传给
  `__libc_start_main`。若不清零，glibc 会把旧堆里的 `envp` 地址注册为退出回调，
  在 `__run_exit_handlers` 中 `call *%rax` 跳入旧地址导致 SIGSEGV（与 shell
  `echo $?` 观测到的 139 一致）。
- 其余架构（aarch64/riscv/loongarch64）同样清零全部 GPR `x`/`regs`，避免旧程序
  参数（如 exec 的 `envp` 落在 aarch64 `x2`、riscv/loongarch64 `a2`）泄漏到新
  镜像，并清除 syscall restart 状态使新程序不被当作 syscall 入口。

复位只覆盖通用寄存器；`cs/ss/rflags`、`spsr`、`sepc/era/elr`、`sp`、TLS 等框架
字段由 `set_ip`/`set_sp`/`set_tls` 调用方重建。

### 为什么 RISC-V FP 状态使用 lazy save/restore

RISC-V 的 `sstatus.FS` 字段跟踪 FP 状态（Off/Initial/Clean/Dirty）。
`FpState::switch_to` 利用这一机制：仅当 `FS::Dirty` 时保存，
恢复时根据目标状态决定是恢复、清零还是跳过。避免不必要的 FP 寄存器操作。

## Drop / 资源释放

本 crate 的类型主要为寄存器快照和上下文容器，不持有需要显式释放的资源。
唯一的 RAII 守卫是 `ExceptionContextGuard`，在 `Drop` 时尝试恢复安装时记录的
per-CPU 活跃 trapframe 指针为前一个值。调度器在当前 CPU 切离安装该指针的任务前
会挂起并清空本 CPU slot，任务被切回后恢复到新的当前 CPU，避免旧 CPU stale 标记
和恢复后 trap handler 后半段缺失异常上下文标记。
