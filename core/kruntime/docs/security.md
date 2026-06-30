# kruntime — 安全与可靠性分析

## 概述

`kruntime` 包含内核启动关键路径上的 `unsafe` 代码：链接器符号、`.init_array` 函数指针调用、`extern "C" main`、从核栈与 SMP 屏障。错误的不变量或损坏的 `.init_array` 可能导致 UB 或任意代码执行。本模块不直接处理用户输入，但决定何时启用中断、何时进入应用 `main`。

## 信任模型

```
kernel-boot / 平台引导
   │ 提供有效 BootInfo、有效 PRIMARY/SECONDARY 入口
   ▼
┌─────────────────────────────────────────────┐
│  kruntime::rust_main / rust_main_secondary   │
│                                              │
│  ┌── unsafe / 链接器边界 ─────────────────┐ │
│  │ extern "C" main                         │ │
│  │ _stext / _etext (ZST 取址)              │ │
│  │ init_cb: .init_array 函数指针调用        │ │
│  │ SecondaryBootStacks::stack_top + boot_ap│ │
│  └─────────────────────────────────────────┘ │
└─────────────────────────────────────────────┘
   │
   ▼
entry::main (应用：entry runtime glue、用户态 init)
```

- **引导层**保证 `BootInfo` 在 `rust_main` 可读且与物理内存描述一致。
- **链接脚本**保证 `_stext`/`_etext`、`__init_array_*` 符号存在且 `.init_array` 仅含有效函数指针。
- **`entry`** 保证 `main` 符号唯一且实现为 `extern "C"` 约定。

## unsafe 代码清单

### 1. `extern "C" { fn main(); }` 与 `unsafe { main() }`（`lib.rs`）

**不变量**：链接镜像中恰好有一个 `#[no_mangle] fn main()`（由 `entry` 提供）；`main` 按 C ABI 调用且不会返回后破坏栈（实际 `main` 应 `shutdown` 或 `!` 语义结束）。

**风险**：若缺少 `main` 符号则链接失败；若存在多个则链接错误。若 `main` 返回后继续执行 `ktask::exit(0)`，行为依赖调度器已初始化。

### 2. 链接符号 `_stext` / `_etext`（`lib.rs`）

```rust
unsafe extern "C" {
    safe static _stext: [u8; 0];
    safe static _etext: [u8; 0];
}
```

**不变量**：符号由链接脚本定义；仅使用 `.as_ptr()` 取 `.text` 边界，不解引用 ZST 存储。

**用途**：配置 `backtrace::init` 的指令指针范围。

### 3. `init_setup::init_cb`（`init_setup.rs`）

**不变量**：

- `__init_array_start` ≤ `__init_array_end`，且区间落在只读、已映射的 `.init_array` 段。
- 段内每个字是指向有效 `extern "C" fn()` 的指针（由 `#[register_init]` 生成）。
- 在单线程、主核、已关本地 IRQ 的上下文中调用（当前由 `rust_main` 在 `init_interrupt` 之后调用，此前未 `enable_local_irq`）。

**风险**：若段被污染，可能跳转到任意地址（高危）。

### 4. `SecondaryBootStacks::stack_top` 中的栈指针（`mp.rs`）

```rust
SECONDARY_BOOT_STACKS.stack_top(...)
```

**不变量**：`SecondaryBootStacks` 在 `.bss.stack`，大小 `TASK_STACK_SIZE`；仅主核在 AP 启动前通过 `stack_top()` 计算栈顶；每个 AP 使用独立槽位，包装类型不向外暴露内部数组引用。

### 5. `lang_items` panic handler

调用 `backtrace::Backtrace::capture()` 与 `khal::power::shutdown()`。要求 panic 时栈可遍历、关机例程可安全执行（可能已在错误状态）。

### 6. `crate_interface` 实现

`dma_integration`、`LogIfImpl` 为 safe Rust；安全性委托给 `memspace::protect`、`khal::console` 等被调用方。

## 内存与并发不变量

1. **主核写 `INITED_CPUS`** 与从核 `fetch_add` 使用 `Release`/`Acquire`，保证 `main()` 前可见所有从核 init 完成。
2. **`ENTERED_CPUS`** 仅用于主核等待 AP 进入 `rust_main_secondary`，与 `INITED_CPUS` 分离，避免启动顺序死锁。
3. **`init_interrupt` 末尾 `enable_local_irq`**：此前定时器处理程序已注册；之后才可能触发抢占与并发 `register_init` 回调之外的 IRQ 路径（`init_cb` 在其之前）。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | `.init_array` 槽位非函数指针， `init_cb` 跳转到垃圾地址 | 高 | 链接脚本错误、内存破坏、恶意对象 | 仅信任 `register_init` 生成的条目；链接器 KEEP `.init_array`；主核单线程阶段调用 |
| T-02 | `BootInfo` 伪造或损坏导致错误内存映射 | 高 | 引导层 bug 或虚拟化攻击 | `khal::firmware::init` 校验；保留区标记；`memspace` 独立管理 |
| T-03 | SMP 屏障失效，从核未 init 完即执行 `main` | 中 | `INITED_CPUS` 计数错误或 `CPU_NUM` 配置不匹配 | 原子计数 + `is_init_ok` 自旋；`CPU_NUM` 与 DT/平台一致 |
| T-04 | 从核栈溢出或 AP 启动过早使用未初始化栈 | 高 | `boot_ap` 与栈注册竞态 | `ENTERED_CPUS` 按序握手；每 AP 独立 `SecondaryBootStacks` 槽 |
| T-05 | `main()` 返回后进入 `ktask::exit` 时调度器状态不一致 | 中 | `entry::main` 异常返回 | 约定 `main` 以关机/进程结束结束；文档说明 |
| T-06 | Panic handler 中 backtrace 遍历无效栈 | 中 | 栈损坏、FP 范围配置错误 | `fp_range` 覆盖线性映射；panic 后关机 |
| T-07 | `DmaPageTableIf::protect` 错误修改内核页表 | 高 | `kdma` 传入非法 vaddr/size | 实现委托 `memspace::protect`；由 mm 子系统校验 |
| T-08 | 日志适配在 init 完成前泄露错误 CPU/task ID | 低 | `is_init_ok` 为 false 时查询 | `cpu_id`/`task_id` 返回 `None` 或默认值 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `init_allocator` 无 FREE 区域 | 固件内存表错误 | `global_init` 失败或 panic | 内核无法启动 | 1 | 启动早期 `expect`/`panic` 快速失败 |
| F-02 | `register_boot_console` VA/PA 偏移不一致 | BootInfo 填错 | `assert` 失败 | 启动中止 | 2 | 显式 `assert_eq` 对齐检查 |
| F-03 | 某从核未到达 `INITED_CPUS` 递增 | AP 启动失败、固件 hang | 主核在 `is_init_ok` 自旋 | 系统卡死，无 `main` | 2 | 启动日志；平台 `boot_ap` 调试 |
| F-04 | `register_init` 回调 panic | 子系统 init bug | 可能中止 `init_cb` 中途 | 后续 init 未执行 | 2 | 回调应保持简短；避免 panic |
| F-05 | 定时器 IRQ 在 `init_interrupt` 前触发 | 平台过早开中断 | 未定义处理 | 可能 panic/挂死 | 2 | 在注册 handler 后再 `enable_local_irq` |
| F-06 | `heapless` 区域日志摘要溢出 | 过多 distinct 区域名 | `expect("too many...")` panic | 启动日志阶段失败 | 3 | `MAX_REGION_LOG_SUMMARIES = 64` |
| F-07 | `main` 符号缺失 | 未链接 `entry` | 链接错误 | 无法生成镜像 | 2 | 构建期发现 |

## 故障管理

- **启动失败**：多数路径使用 `expect`/`panic!` 立即停机，符合内核“早失败”策略；panic handler 打印 backtrace 后 `shutdown`。
- **无运行时降级**：不在此 crate 实现部分启动；驱动 feature 关闭则跳过对应 init，属配置而非故障恢复。
- **SMP 卡死**：主核在 `is_init_ok` 无限自旋，需看门狗（`watchdog` feature）或外部复位。

## 隐私分析

`kruntime` 不处理用户数据。启动日志可能打印内存区域、物理地址范围；`entry::main` 之后的用户态行为不在本 crate 范围。日志中的 `task_id` 仅在 `is_init_ok` 后暴露当前任务 ID。

## 已知限制

1. **`init_cb` 无单条回调隔离**：一个 `register_init` panic 可能中断整表遍历（取决于 panic 策略）。
2. **主核 `main` 前长时间关中断**：`init_interrupt` 注册后才开 IRQ；此前仅 boot 打印路径。
3. **从核永不执行 `main`**：用户态 init 仅在主核；设计如此，非对称 MP 模型。
4. **区域日志摘要启发式**：名称以 `uefi ` 开头且数量 ≥ 8 的区域合并打印，可能隐藏个别异常条目。

## 其它说明（模板章节）

| 章节 | 说明 |
|------|------|
| 基线 | 以本仓库 `docs/ai/skills/module-docs/SKILL.md` 及 `AGENTS.md` 为准 |
| 冗余设计 | 无 |
| 过载控制 | 无（见 `ktask` / `kalloc`） |
| 人因差错 | 无直接用户交互 |
| 故障预测预防 | 无 |
| 升级不中断业务 | 无 |

## 审计清单

修改 `kruntime` 时需验证：

- [ ] 新增 `unsafe` 块附有 `SAFETY:` 或等价说明。
- [ ] 调整 `rust_main` 顺序时评估 IRQ、SMP、`init_cb` 依赖。
- [ ] 修改 `INITED_CPUS` / `ENTERED_CPUS` 语义时检查死锁与屏障。
- [ ] 新增 `register_init` 回调保持短、不可 panic，或接受启动风险。
- [ ] 新增 `crate_interface` 实现保持单一链接实例。
- [ ] Feature 组合在 defconfig 中可构建（`entry` + `kfeat`）。
