# kruntime — 安全与可靠性分析

## 信任模型

`kruntime` 是平台引导层和内核运行时之间的可信编排层。它不直接解析 syscall 参数或用户指针，但它接收 boot 层传入的 `BootInfo`，安装早期内存、调度、中断和驱动运行环境，并在所有已发现 CPU 完成 runtime 初始化后调用 `SystemInitEntry::enter()`。

```
kernel-boot / 平台固件
   │ BootInfo*、主核入口、从核入口
   ▼
┌──────────────────────────────────────────────────┐
│ kruntime                                          │
│   rust_main                                       │
│     early runtime / scheduler / IRQ               │
│     spawn PID-less late_init thread               │
│   late_init_main                                  │
│     drivers / fs / net / SMP / .init_array        │
│     INITED_CPUS barrier                           │
│     SystemInitEntry::enter()  -> spawn PID 1      │
│   rust_main_secondary                             │
│     AP runtime init -> INITED_CPUS -> run_idle    │
└──────────────────────────────────────────────────┘
```

- **boot 层 / 固件**必须提供可读且语义正确的 `BootInfo`、CPU 拓扑、内存区域和可选 boot console MMIO 描述。
- **链接脚本和注册宏**必须提供有效的 `_stext` / `_etext`、`__init_array_start` / `__init_array_end`，并保证 `.init_array` 只包含 `extern "C" fn()` 条目。
- **`kiface` exactly-one provider**保证 `PrimaryKernelEntry`、`SecondaryKernelEntry`、`LoggerAdapter`、`DmaPageTableIf` 和 `SystemInitEntry` 在链接期只有一个实现。
- **`entry` provider**负责策略层启动：`SystemInitEntry::enter()` 必须发布并激活 PID 1 init 任务后返回。

## 外部边界 / 攻击面

`kruntime` 的外部输入都来自内核可信边界内，但错误会在启动早期放大为全局失效：

| 边界 | 输入 | 风险 | 当前约束 |
|------|------|------|----------|
| `BootInfo` | CPU ID、内存表、boot console PA/VA/size | 错误内存映射、错误 CPU 初始化、MMIO 区域越界 | `khal::firmware::init`、`khal::mem::init` 和 `memspace` 负责校验；boot console VA/PA 页内偏移由 `assert_eq!` 检查 |
| 链接符号 | `_stext`、`_etext`、`.init_array` 边界 | backtrace 范围错误、构造函数表越界、任意跳转 | 只取 ZST 符号地址；`.init_array` 通过链接脚本和 `#[register_init]` 约束 |
| SMP bring-up | DT/ACPI 发现的 present CPU、AP 栈、`boot_ap` | 从核使用错误栈、AP 未启动导致主核自旋 | 每个 AP 使用独立 boot stack；`SCHED_READY_CPUS` 串行握手（等 AP 注册 run queue）；全局完成由 `INITED_CPUS == nr_cpus()` 判断 |
| 中断和 timer | softirq runner、timer/IPI/PMU IRQ 号与 handler | 未安装 runner 或未注册 handler 时中断 bottom-half、timer deadline 错乱 | `init_interrupt` 先安装 softirq runner 和注册 handler 再 `enable_local_irq()`；从核在完成本地 runtime 初始化和全局屏障后才开 IRQ |
| NMI 机制 | GIC 版本 / FEAT_NMI 探测与每 CPU 使能（`khal::nmi::early_init` / `late_init`） | 机制误判导致无效寄存器访问，或 watchdog 误报 hardlockup | 平台 `detect_mode()` 门控；`NmiMode::None` 时 watchdog 启动期禁用硬锁检测；所有 NMI 寄存器访问均有 readiness 门控 |
| DMA 页表属性 | `kdma` 传入的内核 VA、size、flags | 错误修改内核页表属性 | `DmaPageTableIf::protect` 委托 `memspace::kernel_layout().protect()` 进行范围和页表处理 |

本 crate 不直接处理用户内存、用户提供指针、DMA buffer 内容或设备寄存器读写；相关 trust boundary 位于 `memspace`、`kdma`、`kdriver` 和具体 HAL/driver 中。它会注册 boot console 的固定 MMIO 映射元数据，但不直接访问该 MMIO。

## unsafe 代码清单

### 链接文本边界（`src/lib.rs`）

`rust_main` 声明 `_stext` / `_etext` 为零长静态符号，并只用 `.as_ptr()` 构造 backtrace 的指令地址范围。

不变量：

- 链接脚本必须定义两个符号，且 `_stext <= _etext`。
- 代码只读取符号地址，不解引用零长对象。
- backtrace 的 frame-pointer 范围从 `PAGE_OFFSET` 到 `usize::MAX`，覆盖主核 task stack 和从核线性映射 boot stack。

### `.init_array` 遍历（`src/init_setup.rs`）

`init_cb()` 将 `[__init_array_start, __init_array_end)` 解释为 `extern "C" fn()` 切片并逐个调用。

不变量：

- 链接脚本导出的起止地址形成同一连续 `.init_array` 区间。
- 区间按函数指针对齐，长度是函数指针大小的整数倍。
- 每个槽位由 `#[register_init]` 等受信代码生成，指向有效 `extern "C" fn()`。
- 调用发生在 late-init bootstrap 线程中：调度器和本地 IRQ 已启用，但 PID 1 尚未 spawn；回调必须不依赖用户态 init 已存在。

风险：该表一旦被错误链接或内存破坏，`init_cb` 可能跳转到任意地址，属于高危启动期边界。

### 从核 boot stack（`src/mp.rs`）

`SecondaryBootStacks` 使用 `UnsafeCell<[[u8; TASK_STACK_SIZE]; NR_CPUS - 1]>` 保存从核启动栈，`stack_top()` 通过 raw pointer 计算每个 AP 的栈顶地址，并为包装类型手写 `Sync`。

不变量：

- `start_secondary_cpus()` 串行分配 `secondary_cpu_index`，并在 `secondary_cpu_index < NR_CPUS - 1` 时才取栈。
- backing array 位于 `.bss.stack`，生命周期覆盖整个内核运行期。
- 每个从核只获得一个独立槽位的 one-past-end 栈顶地址，模块不暴露内部数组引用。
- `SCHED_READY_CPUS` 握手确保主核在启动下一个 AP 前，当前 AP 已进入 runtime 并注册其 run queue，因而主核进入 `init_drivers` 时跨核 spawn 不会命中未注册 RQ。

### per-CPU timer deadline（`src/lib.rs`）

`init_interrupt()` 内的 `NEXT_DEADLINE.read_current_raw()` / `write_current_raw()` 访问当前 CPU 的 per-CPU slot。

不变量：

- timer IRQ handler 在当前 CPU 上运行，且 IRQ/preemption 语义保证访问期间不会迁移到其他 CPU。
- 每个 CPU 只读写自己的 `NEXT_DEADLINE` slot；跨 CPU 不共享该变量。
- `khal::time::arm_timer(deadline)` 使用根据当前单调时间计算的 deadline，避免因 handler 延迟导致立即重复触发。

### kernel-mode unit test raw memory access（`src/mp.rs`，`#[cfg(all(feature = "smp", unittest))]`）

SMP TLB shootdown 单测使用 volatile 读写验证远端 CPU 是否看到页表更新。

不变量：

- 测试页由测试独占分配并在清理阶段释放。
- 测试虚拟地址在读写前已映射到对应物理页。
- 相关 unsafe 仅在 `unittest` 配置下参与构建，不属于生产启动路径。

### panic handler（`src/lang_items.rs`）

panic handler 本身是 safe Rust，但在已损坏栈或已损坏页表上捕获 backtrace 可能再次失败。当前策略是打印 panic 信息和 backtrace，然后调用 `khal::power::platform_power_off()` 终止系统。panic 路径刻意绕过 `khal::power::power_off()` 的 SMP stop 钩子：钩子通过 IPI 停机，而 panic CPU 可能正持有其他 CPU 自旋等待的锁，走钩子会死锁。

## 内存安全不变量

- `BootInfo` 在 `rust_main(arg)` 期间必须可读，且其内存表描述的 FREE/RSVD/设备区域不能互相矛盾。
- boot console 固定映射的 PA/VA 页内偏移必须一致；否则 `register_boot_console_runtime_region()` 立即 panic，避免注册错误映射。
- `.init_array` 边界必须位于已映射内核镜像中，并且不会被可写数据覆盖。
- 从核 boot stack 数组必须至少覆盖被实际启动的 AP 数量；超过 `NR_CPUS - 1` 的 present CPU 不会被 `kruntime` 启动。
- `memspace::init_memory_management()` 之后，`DmaPageTableIf::protect()` 只能通过 `memspace` 持有内核地址空间锁来修改映射属性。
- PID-less late-init 线程和普通内核 worker 不进入 root PID allocator；PID 1 只能由 `SystemInitEntry::enter()` 中的 init spawn 消耗并断言。

## 线程安全

- `INITED_CPUS` 使用 `Release`/`Acquire` 作为全局 runtime-ready 屏障。主核 late-init 线程和每个从核完成本地 runtime 初始化后递增；`SystemInitEntry::enter()` 和依赖跨 CPU 调度的 late-start worker 只在屏障满足后运行。
- `SCHED_READY_CPUS` 只表达 AP 已注册 run queue（进入 runtime 后进一步完成内存管理与调度器初始化），不代表 AP 初始化完成；它与 `INITED_CPUS` 分离，避免把“可跨核调度”/“可启动下一个 AP”和“全局 runtime ready”混在一起。
- `LoggerAdapter::cpu_id()` 和 `task_id()` 在 `is_init_ok()` 前避免读取可能未就绪的 current task / CPU-local 运行时状态；非 SMP 构建下 CPU ID 固定为 0。
- 从核在 `INITED_CPUS` 屏障后才启用 IPI/PMU IRQ、本地 IRQ 和 watchdog，并最终进入 `ktask::run_idle()`。
- `DmaPageTableIf::protect()` 通过 `memspace::kernel_layout().lock()` 串行化内核页表属性更新。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | `.init_array` 槽位不是有效函数指针，`init_cb` 跳转到非法地址 | 高 | 链接脚本错误、恶意/损坏对象文件、内存破坏 | 仅信任注册宏生成条目；链接脚本保留并界定 `.init_array`；启动期集中调用 |
| T-02 | `BootInfo` 内存表或 CPU 拓扑错误 | 高 | boot 层 bug、固件表异常、虚拟化环境错误 | 由 `khal::firmware` / `khal::mem` / `kcpu_id_map` 建立运行时视图；后续初始化只消费该视图 |
| T-03 | AP 未完成初始化时进入系统 init 或跨 CPU worker | 中 | `INITED_CPUS` 计数错误、CPU 数来源错误 | 跨核 spawn 由 `SCHED_READY_CPUS` 提前守护；系统 init 入口等待 `INITED_CPUS == kcpu_id_map::nr_cpus()`；计数均使用 Release/Acquire |
| T-04 | 从核使用错误或重叠 boot stack | 高 | AP 启动循环越界、并发启动复用槽位 | `secondary_cpu_index < NR_CPUS - 1`；串行 `boot_ap` + `SCHED_READY_CPUS` 握手；每 AP 独立槽 |
| T-05 | PID 1 被提前消耗 | 中 | late-init 线程或后台 worker 获得 Linux-visible identity；`SystemInitEntry` 未先创建 init | boot、idle、late-init 和普通内核 worker 使用 PID-less identity；init 创建路径断言 root PID 为 1 |
| T-06 | IRQ 在 handler 注册前启用 | 高 | `init_interrupt` 顺序被改坏、平台提前开 IRQ | 主核先安装 softirq hardirq-exit runner，再注册 timer/IPI/PMU handler，最后开本地 IRQ；从核初始化完成后再开 IRQ |
| T-07 | per-CPU timer deadline raw access 在可迁移上下文运行 | 中 | handler 被改为普通线程调用或抢占语义变化 | unsafe 注释要求 IRQ/preemption 禁止迁移；审计 raw percpu 调用点 |
| T-08 | `DmaPageTableIf::protect` 修改非法内核 VA 范围 | 高 | `kdma` 调用方传入错误地址或长度 | 委托 `memspace` 的内核 layout 锁和页表校验 |
| T-09 | panic handler 在损坏栈上再次失败 | 中 | 栈溢出、页表损坏、backtrace 范围错误 | panic 后只做诊断并关机，不尝试恢复 |
| T-10 | 启动日志泄露物理地址和设备映射 | 低 | 开启启动日志 | 限于内核日志；不处理用户数据，但部署时需按日志策略限制可见性 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 全局 allocator 没有可用 FREE 区域 | 固件内存表错误或 boot 层漏报 | allocator 初始化不完整 | 早期分配失败或 panic，系统无法启动 | 1 | 启动期快速失败；依赖 boot/mem 子系统校验 |
| F-02 | boot console MMIO VA/PA 页内偏移不一致 | `BootInfo` 填写错误 | `assert_eq!` 失败 | 启动中止 | 2 | 显式偏移检查后才注册 fixed device region |
| F-03 | 某从核未递增 `SCHED_READY_CPUS` | `boot_ap` 失败、固件未唤醒 AP、AP 在 memspace/scheduler 初始化期 trap | 主核卡在 AP bring-up | late init 无法继续 | 2 | `start_secondary_cpus()` 返回 `boot_ap` 错误；启动期 hang 依赖平台日志/调试 |
| F-04 | 某 CPU 未递增 `INITED_CPUS` | 从核 init hang、主核 late init panic | 全局 ready 屏障不满足 | 系统 init 不启动或从核不进 idle | 2 | 启动日志；watchdog feature 或外部复位 |
| F-05 | `register_init` 回调 panic | 子系统 init bug | `.init_array` 遍历中断 | 后续 init 不执行，panic 后关机 | 2 | 回调保持短小、无阻塞；新增回调需审计 panic 路径 |
| F-06 | `SystemInitEntry` provider 缺失或重复 | `entry` 未提供实现或多个实现冲突 | 链接失败 | 无法生成镜像 | 2 | `kiface` exactly-one provider 构建期发现 |
| F-07 | PID 1 断言失败 | Linux-visible task 在 init 前抢先消耗 root PID | `assert_eq!` panic | 启动中止 | 2 | boot、idle、late-init 和普通内核 worker 使用 PID-less identity |
| F-08 | region 日志摘要数组溢出 | distinct region name/flags 超过 64 | `expect("too many region log summaries")` panic | 启动日志阶段失败 | 3 | `MAX_REGION_LOG_SUMMARIES = 64`；异常平台需扩大上限或调整汇总策略 |
| F-09 | timer handler 重复立刻触发或 deadline 倒退 | 时间源异常或 deadline 更新逻辑被改坏 | tick 过密、调度异常 | 性能下降或 hang | 2 | deadline 取当前记录与 `now + interval` 的较大值，再写入下一 tick |
| F-10 | NMI 机制探测或每 CPU 使能失败 | 平台缺 FEAT_NMI / GIC 版本过低 / 初始化顺序错误 | 本 CPU 无 NMI 或 hardlockup 被禁用 | 系统失去硬锁检测（软锁仍在） | 3 | 启动日志 + `mode()` / `enable_periodic_nmi` 返回值显式降级 |

## 故障管理

- **启动期错误快速失败**：关键路径使用 `assert!`、`expect` 或 `panic!`，避免在未完整初始化的内核中降级运行。
- **panic 后关机**：panic handler 打印 panic 信息和 backtrace 后调用裸平台终点 `khal::power::platform_power_off()`（不走 SMP stop 钩子，避免 panic 持锁导致 IPI 停机死锁）；本 crate 不尝试恢复。
- **SMP hang 无本地超时**：`SCHED_READY_CPUS` / `INITED_CPUS` 等待使用自旋，若 AP 或主核 late init 卡死，需要 watchdog feature、平台日志或外部复位介入。
- **feature 关闭是配置选择**：如 `fs`、`net`、`watchdog`、`pmu` 未启用时跳过对应初始化，不作为运行时故障处理。

## 隐私分析

`kruntime` 不处理用户数据、文件内容或网络 payload。它可能在启动日志中打印：

- `BootInfo` 地址；
- CPU ID；
- 物理内存区域和保留区来源；
- runtime device/iomap 的 PA/VA 范围；
- init 完成后的 task owner key。

这些信息对内核调试有价值，但在面向非可信操作者的产品配置中应按系统日志策略限制可见性。

## 已知限制

1. **`.init_array` 无单条隔离**：一个回调 panic 会中断后续回调；没有 per-callback recovery。
2. **SMP 等待无超时**：AP 启动或 init 屏障失败会导致自旋等待。
3. **系统 init 仅由主核 late-init 线程触发**：从核初始化完成后只进入 idle，不执行 `SystemInitEntry`。
4. **PID 1 语义依赖 `SystemInitEntry` 合约**：`enter()` 必须创建、发布并激活 root PID 1 init。
5. **启动日志摘要是启发式**：只有名称以 `uefi ` 开头且数量达到阈值的区域会汇总，可能隐藏单个区域的细节。

## 审计清单

修改 `kruntime` 时需验证：

- [ ] 新增或移动 `unsafe` 块时，同步说明真实不变量、调用上下文和失败影响。
- [ ] 调整 `rust_main` 顺序时，重新检查 allocator、memspace、driver resource provider、logger、scheduler、IRQ 的依赖。
- [ ] 调整 `late_init_main` 顺序时，重新检查 `.init_array`、SMP ready 屏障、vsock/TIPC bridge 和 `SystemInitEntry` 的顺序。
- [ ] 修改 `INITED_CPUS` / `SCHED_READY_CPUS` 时，区分“AP 已可跨核调度（run queue 注册）”和“所有 CPU runtime ready”，并检查 Release/Acquire 配对。
- [ ] 新增 early/late kernel thread 时，确认它是否应为 PID-less `Internal`，不能抢先消耗 PID 1。
- [ ] 修改 `SystemInitEntry` provider 时，确认它 spawn 并激活 PID 1 后返回。
- [ ] 新增 `register_init` 回调时，确认它不依赖用户态 init、避免长时间阻塞，并接受 panic 会中止启动。
- [ ] 修改 timer/IPI/PMU handler 时，确认 handler 注册先于 IRQ enable，raw per-CPU 访问仍只发生在不可迁移上下文。
- [ ] 修改 DMA protect 接线时，确认仍由 `memspace` 统一持锁和校验内核页表范围。
- [ ] 修改启动日志时，评估是否新增物理地址、设备映射或任务身份泄露。
