# kcpu — 安全与可靠性分析

## 概述

`kcpu` 是 x-kernel 中直接操作 CPU 特权级硬件的底层 crate，涉及大量
`unsafe` 代码：naked 函数、内联汇编、裸指针操作和 `transmute`。不正确的
使用或不变量破损可能导致未定义行为、内存损坏或特权级泄露。

## 信任模型

```text
内核上层（调度器/信号/syscall/驱动）
   │
   │ safe API: init_trap(), UserContext::run(),
   │           TaskContext::switch_to(), user_copy()
   │
   v
┌──────────────────────────────────────────────────┐
│  kcpu                                            │
│                                                  │
│  ┌── unsafe 边界 ──────────────────────────────┐ │
│  │ 汇编入口 (excp.S / copy_user.S)             │ │
│  │ naked 函数 (context_switch, fpstate_*)      │ │
│  │ 内联汇编 (movgr2fr.d / movfr2gr.d)         │ │
│  │ 裸指针操作 (异常表、栈帧构造)               │ │
│  │ transmute (IDT 填充、寄存器数组转换)        │ │
│  │ MSR/CSR 写入 (syscall 配置、页表切换)       │ │
│  └─────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────┘
```

- **safe API 调用者**信任 `kcpu` 正确维护寄存器布局和硬件操作不变量。
- **汇编代码**被视为可信边界内的实现细节，其正确性通过人工审计和架构约定保证。
- **`user_copy` 调用者**信任异常表机制能正确拦截无效用户指针。

## 外部边界 / 攻击面

本模块直接接触以下外部边界：

- **用户内存**：`copy_user.S` 中的 `user_copy` 函数从用户空间地址读写数据。
  用户可提供任意地址（包括未映射、只读或内核地址），异常表机制用于安全恢复。

- **MMIO / CSR / MSR 寄存器**：trap handler 读取 CR2（x86_64）、
  FAR_EL1（aarch64）、stval（riscv）、badv（loongarch64）获取故障地址。
  MSR 写入配置 syscall 入口（x86_64 LStar/Star/SFMask/Efer）。

- **用户态寄存器状态**：`UserContext::run()` 切换到用户态后，用户可控制
  所有通用寄存器内容。trap handler 需正确处理任意寄存器值。
- **riscv64 `gp`（percpu 基址）**：`gp` 是每 CPU 不变量，绝不从每任务
  trapframe 恢复。trap 途中任务可能因 handler 阻塞而迁移到其他 hart；若从
  可迁移的 trapframe 恢复 `gp`，会把另一 hart 的 percpu 基址装入本 hart，
  使 `current()`/`this_cpu_id()` 指向错误的 CPU 与任务，构成跨任务指针泄漏。
  当前实现：S-mode trap 返回不触碰 `gp`；仅 U-mode 返回从 slot-3 恢复用户
  `gp`。

- **中断/异常输入**：硬件中断和异常向量触发 trap 入口。IRQ 编号来自硬件，
  不完全可信。

- **指令流**：LoongArch64 的 `emulate_unaligned()` 读取故障地址处的指令
  字进行解码。该指令字来自用户或内核代码段。

## unsafe 代码清单

### 1. `ExceptionContextGuard::new` / `Drop`（`active_exception_context.rs`）

```rust
ACTIVE_EXCEPTION_CONTEXT_PTR
    .current_ref_raw()
    .swap(ptr, Ordering::Relaxed)
```

**不变量**：`tf` 必须是有效的 `&ExceptionContext` 引用；guard 的生命周期
必须覆盖 trap handler 整个执行期。

**为何安全**：安装时记录原始 per-CPU slot 地址；per-CPU 数据区在整个内核生命
周期内有效，因此该裸指针在迟到 Drop 时仍可解引用。`swap` 在 trap 入口的 IRQ
关闭上下文中执行。Drop 时用 CAS 恢复原始 slot 和迁移后的当前 CPU slot，避免
trap 后端阻塞/迁移后误写或遗漏 CPU slot。CAS 失败表示 slot 已被调度器清空或被
新的 trap 覆盖，保留当前值是正确行为。迁移后的内层 guard 释放时，当前 CPU slot
恢复为 `self.prev` 而不是 0；`self.prev` 来自同一任务中仍存活的外层 guard，必须
保持可见，直到外层 guard 自己释放。

### 2. `active_exception_context()` 裸指针转换（`active_exception_context.rs:39-52`）

```rust
Some(unsafe { *(ptr as *const ExceptionContext) })
```

**不变量**：`ptr` 必须指向有效的 `ExceptionContext` 实例。

**为何安全**：指针仅由 `ExceptionContextGuard::new` 安装，guard 的 `Drop`
在原始 slot 和当前 CPU slot 上尝试恢复；调度器切离安装者任务前会挂起并清空本
CPU slot，切回后恢复到当前 CPU。该 API 立即按值复制 trapframe 并返回快照，
避免向调用者暴露可能在 trap 返回后失效的借用引用。

### 3. `prepare_initial_frame`（`x86_64/ctx.rs:367-379`）

```rust
let frame_ptr = unsafe { top_u64.sub(1).cast::<ContextSwitchFrame>().sub(1) };
unsafe { frame_ptr.write(ContextSwitchFrame { rip: entry as _, ..Default::default() }) };
```

**不变量**：`kstack_top` 指向有效的内核栈顶，栈有足够空间容纳
`ContextSwitchFrame`（56 字节）。

**为何安全**：由 `TaskContext::init()` 调用，调用者保证 `kstack_top` 有效。
x86_64 栈要求 16 字节对齐，`sub(1)` 保证 `ret` 后对齐。

### 4. `context_switch` naked 函数（`x86_64/ctx.rs:376-398`）

```rust
#[unsafe(naked)]
unsafe extern "C" fn context_switch(_current_stack: &mut u64, _next_stack: &u64) {
    naked_asm!("push rbp; push rbx; ... mov [rdi], rsp; mov rsp, [rsi]; ...")
}
```

**不变量**：`rdi` 指向当前任务的 `rsp` 字段，`rsi` 指向下一任务的 `rsp`
字段。callee-saved 寄存器布局与 `ContextSwitchFrame` 一致。

**为何安全**：调用前中断已关闭；保存/恢复序列对称；
`ret` 跳转到 `ContextSwitchFrame.rip` 处的入口函数。

### 5. FP 状态保存/恢复（各架构 ctx.rs）

- x86_64：`_fxsave64` / `_fxrstor64` 内联指令。
- aarch64：`fpstate_save` / `fpstate_restore` naked 函数。
- riscv：`save_fp_registers` / `restore_fp_registers` naked 函数。
- loongarch64：`save_fp_registers` / `restore_fp_registers` naked 函数。

**不变量**：目标内存区域大小和对齐满足架构要求。

**为何安全**：`FpState`/`FpuState`/`FxStateBlock` 的布局通过
`static_assertions` 或 `repr(C, align(...))` 保证与指令期望一致。

### 6. `ExtendedState::default`（`x86_64/ctx.rs:310`）

```rust
FxStateBlock {
    fpu_ctrl: 0x037f,
    sse_mxcsr: 0x1f80,
    st_space: [0; 16],
    xmm_space: [0; 32],
    _padding: [0; 12],
    // ...
}
```

**不变量**：`FxStateBlock` 的所有字段均为 `u16`/`u32`/`u64` 数组，
全零是有效表示。

**为何安全**：`FxStateBlock` 现在通过显式字段初始化构造，避免了对
“整块零初始化后再补写关键字段”的 `unsafe` 假设。其布局仍由
`repr(C, align(16))` 和 `static_assertions` 保证满足 FXSAVE/FXRSTOR 要求。

### 7. `switch_to` 页表切换（各架构 ctx.rs）

```rust
if next_ctx.cr3 != self.cr3 {
    karch::write_user_page_table(next_ctx.cr3);
}
```

**不变量**：`cr3`/`ttbr0_el1`/`satp`/`pgdl` 必须指向有效的页表根。

**为何安全**：`TaskContext` 中的页表根由内核页表管理子系统提供，
通过 `set_page_table_root()` 设置。

### 8. IDT 初始化中的 `transmute`（`x86_64/idt.rs:25-29`）

```rust
let entries = unsafe {
    core::mem::transmute::<&mut InterruptDescriptorTable, &mut [Entry<()>; 256]>(&mut table)
};
```

**不变量**：`InterruptDescriptorTable` 的内存布局与 `[Entry<()>; 256]` 一致。

**为何安全**：`InterruptDescriptorTable` 文档声明其为 256 个 `Entry` 的
透明包装。`const NUM_INT: usize = 256` 保证尺寸匹配。

### 9. `init_syscall` MSR 写入（`x86_64/userspace.rs:169-191`）

```rust
LStar::write(...); Star::write(...); SFMask::write(...);
Efer::update(|efer| *efer |= EferFlags::SYSTEM_CALL_EXTENSIONS);
```

**不变量**：`syscall_entry` 地址必须在内核代码段内；段选择子指向正确的
GDT 描述符。

**为何安全**：`syscall_entry` 由 `excp.S` 定义，链接到内核地址空间。
段选择子使用 `gdt.rs` 中的编译时常量。

### 10. 异常表操作（`userspace_common.rs:52-90`）

```rust
let entries = unsafe {
    core::slice::from_raw_parts(_ex_table_start.as_ptr(), count)
};
```

**不变量**：`_ex_table_start` 和 `_ex_table_end` 由链接脚本定义，
标记 `__ex_table` 段的起止；条目数量为非负值。

**为何安全**：符号由链接器生成，在 `init_exception_table()` 中排序后
仅做只读访问。`fixup_exception` 中的二分查找不修改条目。

### 11. LoongArch64 `emulate_unaligned`（`loongarch64/unaligned.rs:574-636`）

```rust
let badi = unsafe { core::ptr::read(self.era as *const u32) };
let regs = unsafe {
    core::slice::from_raw_parts_mut(
        core::ptr::from_mut(&mut self.regs).cast::<usize>(),
        32,
    )
};
```

**不变量**：

- `self.era` 必须指向可读的内存地址（故障指令所在位置）。
- `GeneralRegisters` 的内存布局必须与 `[usize; 32]` 一致。
- `rd`（目标寄存器编号）在 0–31 范围内。
- 非对齐读写的目标地址 `badv` 可安全访问。

**为何安全**：在 trap handler 上下文中调用，`era` 指向故障指令所在的
代码段。`GeneralRegisters` 为 `repr(C)` 结构体，32 个 `usize` 字段连续排列。
寄存器编号从指令字的低 5 位提取，天然在 0–31 范围内。

### 12. LoongArch64 FP 寄存器读写（`loongarch64/unaligned.rs:67-533`）

```rust
unsafe { asm!("movgr2fr.d $f0, {val}", val = in(reg) val) }
unsafe { asm!("movfr2gr.d {val}, $f0", val = out(reg) value) }
```

**不变量**：每条汇编指令操作固定的 FP 寄存器编号，与 `write_fpr`/`read_fpr`
的 match 分支一一对应。

**为何安全**：LoongArch64 ISA 中 `movgr2fr.d` / `movfr2gr.d` 是通用寄存器
与 FP 寄存器之间的数据搬移指令，不涉及内存访问。64 个函数各自硬编码了
$f0–$f31 的寄存器编号。

## 内存安全不变量

以下不变量必须在任何时候都成立：

1. **栈帧布局一致性**：汇编入口保存的寄存器布局必须与 Rust 侧
   `ExceptionContext` 的 `repr(C)` 布局完全匹配。`trapframe_size` 编译期
   常量注入汇编确保尺寸一致。

2. **页表有效性**：`TaskContext` 中的页表根（CR3/ttbr0_el1/satp/pgdl）
   必须始终指向有效的页表。`switch_to` 写入硬件前不做验证，
   由调用者保证。

3. **异常表已排序**：`fixup_exception` 使用二分查找，要求异常表在调用前
   已通过 `init_exception_table()` 排序。

4. **guard 生命周期与迁移**：`ExceptionContextGuard` 必须覆盖 trap handler
   整个执行期。guard 记录安装时的 per-CPU slot；调度器切离安装者任务前必须挂起并
   清空当前 CPU slot，切回后恢复到当前 CPU。Drop 必须同时处理原始 slot 和迁移后
   的当前 CPU slot，避免 trap handler 阻塞/迁移后留下 stale active-exception 标记
   或在恢复执行后丢失异常上下文标记。嵌套 guard 的 `prev` 指向外层 live guard
   的 `ExceptionContext`；只要外层 guard 未释放，该指针必须被视为当前任务仍处于
   外层异常上下文的标记。

5. **中断关闭**：`context_switch` 和 `UserContext::run` 的核心操作必须在
   中断关闭状态下执行。中断关闭保护了 per-CPU 状态和寄存器保存/恢复的原子性。

6. **naked 函数调用约定**：所有 naked 函数的参数传递必须与汇编中的寄存器
   使用一致（如 x86_64 的 `rdi`/`rsi` 用于 `context_switch`）。

## 线程安全

| 类型 | `Send` 条件 | `Sync` 条件 |
| ---- | ----------- | ----------- |
| `ExceptionContext` | `Copy` 类型，自动 `Send` | 自动 `Sync` |
| `UserContext` | `Copy`/字段均为 `Send` | 自动 `Sync` |
| `TaskContext` | 字段均为 `Send` | 字段均为 `Sync`，但不应跨 CPU 共享 |
| `ExceptionContextGuard` | 不应跨线程转移（绑定安装时 slot） | 不可共享使用 |

- `TaskContext` 的 `switch_to` 方法接受 `&mut self`，天然独占。
  上下文切换在关中断下执行，防止并发访问。
- `ACTIVE_EXCEPTION_CONTEXT_PTR` 为 per-CPU 变量。安装路径写当前 CPU 副本；
  迟到的 Drop 可能在迁移后访问原 CPU slot，也可能需要清理迁移后当前 CPU 的恢复
  标记，因此恢复路径必须使用 CAS，不能无条件覆盖任一 CPU slot。
- `IRQ` 和 `PAGE_FAULT` 分布式切片为编译期静态数据，运行时只读。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
| ---- | -------- | -------- | -------- | -------- |
| T-01 | 用户通过 `user_copy` 传入恶意地址导致内核 panic | 高 | `user_copy` 访问无效用户指针且异常表条目缺失 | 异常表机制覆盖所有 `copy_user.S` 中的访存指令；`fixup_exception` 拦截内核态 Page Fault 并恢复到安全路径 |
| T-02 | trap frame 布局与汇编不一致导致寄存器错位 | 高 | 修改 `ExceptionContext` 字段顺序但未同步汇编 | `trapframe_size` 编译期常量注入汇编；`repr(C)` 保证内存布局可预测 |
| T-03 | LoongArch64 `emulate_unaligned` 解码未知指令导致寄存器损坏 | 高 | 故障指令为非预期的操作码 | 未匹配的操作码返回 `Err(UnalignedError)`，不修改寄存器；trap handler panic 而非继续执行 |
| T-04 | `active_exception_context()` 返回过期或 stale trapframe | 高 | trap handler 阻塞/迁移后原 CPU slot 未清理，或调用者持久保存快照来源 | API 只返回按值快照；context switch 挂起并清空当前 CPU slot、切回后恢复当前 CPU slot；guard Drop 用 CAS 恢复原始/当前 slot |
| T-05 | `switch_to` 使用无效页表根导致 TLB 填充错误地址 | 高 | `TaskContext` 的页表根被错误设置 | `set_page_table_root` 接受 `HwPageTableRoot` 类型，由内核页表子系统保证有效性 |
| T-06 | IDT/向量表条目指向错误地址 | 高 | 汇编符号名拼写错误或链接脚本问题 | `trap_handler_table` 为汇编定义的固定大小数组；IDT 初始化通过 `set_handler_addr` 填充 |
| T-07 | 中断未关闭时调用 `switch_to` 或 `run` | 中 | 调用者违反调用约束 | `run()` 内部调用 `disable_local_irq()`；`switch_to` 要求调用者保证关中断 |
| T-08 | `ExceptionContextGuard` 嵌套导致 per-CPU 指针被覆盖 | 低 | trap handler 执行期间再次触发异常 | `swap` 语义保存前一个值，guard drop 时用 CAS 恢复；深层嵌套通过 guard 链正确恢复，且不会覆盖后续新 trap |
| T-09 | LoongArch64 用户态构造恶意非对齐指令字 | 高 | 用户态程序执行非对齐访问，故障指令字被 `emulate_unaligned` 解码为意外操作 | `emulate_unaligned` 仅修改指令编码中指定的目标寄存器；未匹配操作码返回错误不修改任何状态；操作码掩码仅匹配已知指令类型 |
| T-10 | `init_trap()` 被重复调用导致硬件状态不一致 | 中 | 二次调用覆盖 GDT/IDT/向量表等已激活的硬件结构 | 由调用者保证仅调用一次（boot 阶段单线程执行）；无运行时防护 |
| T-11 | x86_64 `orig_rax` 被恶意篡改导致 syscall 重启异常 | 中 | 用户态通过信号处理器修改 trap frame 中的 `orig_rax` | `orig_rax` 仅在 `syscall_entry` 汇编中写入；信号帧保存 `UserRestorableContext` 窄状态，不从用户栈恢复 `orig_rax`；x86_64 `EnterUserFrame.kernel_rsp` 也不进入用户信号帧；signal delivery 代码通过 `syscall_restart_error()` 判断重启条件 |
| T-12 | `rollback_syscall` 在非 syscall 上下文调用 | 低 | 信号处理代码误判上下文类型 | `is_from_syscall()` 检查 `orig_rax != u64::MAX`，非 syscall 上下文时 `rollback_syscall()` 和 `restart_with_syscall()` 为 no-op |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
| ---- | -------- | -------- | -------- | -------- | ------ | -------- |
| F-01 | Page Fault 无法处理导致 panic | 页面未映射 + 异常表无匹配条目 + 无注册 handler | 当前 CPU 停止 | 系统崩溃 | 1 | 三级处理：注册 handler → 异常表修正 → panic 附带诊断信息 |
| F-02 | `user_copy` 返回非零值（部分拷贝） | 用户缓冲区部分无效 | 系统调用返回 -EFAULT | 用户进程收到错误，可重试 | 3 | 调用者检查返回值；异常表机制保证不 panic |
| F-03 | `emulate_unaligned` 返回错误 | 故障指令为不支持的 FP 或向量操作 | 非对齐访问失败 | 当前进程可能收到 SIGBUS | 3 | trap handler panic（内核态）/ 返回错误（用户态） |
| F-04 | 上下文切换期间中断导致寄存器损坏 | 调用者未关中断 | 当前任务状态不一致 | 调度器数据损坏 | 1 | API 约束要求关中断调用；`run()` 自行管理中断状态 |
| F-05 | GDT/IDT 初始化顺序错误 | `init_trap()` 调用前 per-CPU 未初始化 | 段寄存器加载失败 | CPU 异常 | 1 | 文档标注前置条件；boot 代码按固定顺序调用 |
| F-06 | FP 状态跨任务泄漏 | `fp-simd` 未启用但任务使用 FP 指令 | FP 寄存器包含前一个任务数据 | 信息泄漏 | 2 | 未启用 `fp-simd` 时不保存/恢复；依赖内核配置禁用用户态 FP |
| F-07 | `ExceptionContextGuard` 提前 drop | trap handler 中 guard 被 shadow 或手动 drop | `active_exception_context()` 返回错误指针 | NMI 看门狗获取过期快照 | 2 | guard 声明为 handler 第一个局部变量，生命周期覆盖整个函数 |
| F-13 | `ExceptionContextGuard` 迟到 drop 写错 CPU slot | trap handler 中阻塞并迁移，Drop 时重新计算当前 CPU | 原 CPU长期保持 stale active-exception 标记，或迁移后当前 CPU 标记未恢复 | `check_preempt_pending()` 一直跳过抢占，或 trap handler 后半段错误允许抢占 | 1 | guard 保存原始 slot 并 CAS 恢复；context switch 挂起/恢复当前 CPU slot |
| F-08 | `emulate_unaligned` 内部 `.unwrap()` panic | 非对齐指令解码成功但实际访问故障（页不存在） | 当前 CPU trap handler panic | 系统崩溃 | 1 | `_unaligned_read`/`_unaligned_write` 汇编辅助函数含异常表条目，通常能安全恢复；若异常表未覆盖则 panic |
| F-09 | trap handler 内 panic 触发 double fault | panic 过程中再次触发异常 | panic unwind 路径访问无效内存 | 系统不可恢复 | 1 | panic handler 应最小化操作；x86_64 #DF 有独立栈（TSS IST） |
| F-10 | `init_trap` 重复调用导致 IDT/向量表重置 | boot 代码逻辑错误 | 中断配置被覆盖 | 后续中断处理异常 | 1 | 前置条件约束（仅调用一次）；无运行时防护 |
| F-11 | `syscall_restart_error` 误判导致 syscall 不必要重启 | 返回值恰好等于 ERESTART 常量但非 syscall 上下文 | 用户态 syscall 被意外重启 | 用户进程行为异常 | 3 | `is_from_syscall()` 前置检查过滤非 syscall 上下文；重启常量为负值，正常返回值不会匹配 |
| F-12 | `rollback_syscall` rip 回退到非法地址 | `rip` 值小于 2（syscall 指令长度） | `saturating_sub` 保证不溢出，但回退后 rip 可能指向非法指令 | 用户进程收到 SIGILL | 3 | `saturating_sub` 防止下溢；用户态 rip 由内核设置，正常情况下不会小于 2 |

## 故障管理

`kcpu` 通过以下机制处理故障：

- **异常表修正**：`user_copy` 的 Page Fault 通过异常表安全恢复，返回
  未拷贝字节数（0 表示成功）。
- **分布式 handler 链**：Page Fault 和 IRQ handler 通过 `linkme` 分布式
  切片注册，按注册顺序尝试处理。
- **panic**：无法恢复的异常（未知向量、未处理的 Page Fault、#GP 等）
  直接 panic，附带完整 trap frame 和 backtrace 信息。
- **`Result` 传播**：LoongArch64 的 `emulate_unaligned` 返回
  `Result<(), UnalignedError>`，允许调用者决定处理方式。

## 隐私分析

本模块不处理用户数据内容。`user_copy` 在内核与用户空间之间搬运数据，
但不解析或记录数据内容。FP 寄存器中可能包含用户态敏感数据，
`fp-simd` feature 启用后通过保存/恢复防止跨任务泄漏。

## 已知限制

1. **trap handler 不支持嵌套**：`ExceptionContextGuard` 使用单个 per-CPU
   指针，深层嵌套异常仅通过 guard 链恢复前一个值，不维护完整栈。

2. **IRQ handler 最多一个**：`dispatch_irq_trap!` 宏仅调用第一个注册的
   handler，多个 handler 时打印警告并忽略后续。

3. **LoongArch64 非对齐模拟范围有限**：仅覆盖基本加载/存储和标量浮点指令，
   不覆盖 SIMD（LSX/LASX）指令的非对齐情况。

4. **`context_switch` 无栈溢出检测**：新任务的内核栈必须有足够空间容纳
   `ContextSwitchFrame`，否则 `prepare_initial_frame` 写越界。

5. **x86_64 不支持 XSAVE**：使用 FXSAVE/FXRSTOR，不支持 AVX 等
   扩展状态。依赖扩展状态的任务会丢失 YMM/ZMM 寄存器。

6. **riscv64 hwprobe snapshot 依赖固件描述**：RISC-V capability snapshot
   只报告 device tree 或内核地址布局中可确定的信息。未描述的 vendor id
   和扩展按 Linux hwprobe ABI 返回 `0` / unknown，避免在 S-mode syscall
   路径读取可能 fault 的 M-mode CSR。

## 审计清单

修改 `kcpu` 时需验证：

- [ ] 修改 `ExceptionContext` 字段后同步更新汇编中的 `trapframe_size` 和
      寄存器保存/恢复序列。
- [ ] 新增 `unsafe` 块均有 `SAFETY:` 注释说明不变量。
- [ ] 修改 `ContextSwitchFrame` 布局后同步更新 `context_switch` naked 函数
      中的 `push`/`pop` 序列。
- [ ] 修改 GDT 段选择子常量后验证 `UserContext::new` 中的 `cs`/`ss` 值。
- [ ] 新增异常向量处理时检查是否需要更新异常表或注册新 handler。
- [ ] LoongArch64 新增指令模拟时验证操作码掩码和寄存器编号范围。
- [ ] 修改 `copy_user.S` / `atomic_user.S` 后确认每条访存指令均有对应的 `_asm_extable` 条目。
- [ ] 新增 per-CPU 变量时验证初始化顺序（percpu init → `init_trap`）。
- [ ] 新增 RISC-V hwprobe key 时确认数据来源是否来自启动期 snapshot，
      并同步 `ksyscall` 聚合/过滤语义。
- [ ] 修改页表切换逻辑后验证 TLB 刷新语义是否正确。
- [ ] 修改 `fp-simd` / `tls` feature 门控代码后验证所有架构的一致性。
- [ ] 修改 x86_64 `orig_rax` 或 syscall 重启逻辑后验证 `excp.S` 中的 push/pop 序列与 `ExceptionContext` 布局一致。
- [ ] 新增可重启 syscall 错误码时同步更新 `syscall_restart_error()` 中的常量列表。
