# kruntime — 设计文档

## 定位

`kruntime` 是 x-kernel 的**运行时编排层**：在平台引导代码将控制权交给内核后，负责主核/从核的初始化顺序、子系统装配，以及跳转到系统初始化入口（`entry` crate 提供的 `SystemInitEntry`）。它不实现具体设备驱动或 syscall，而是把 `khal`、`memspace`、`ktask`、`kdriver` 等按固定阶段串联起来。

目标读者：需要理解启动链、SMP 屏障、或在此 crate 增加初始化钩子的内核开发者。

## 背景

x-kernel 将**引导**（`kernel-boot`、平台 `platconfig`）与**运行时**（内存、调度、驱动、用户态 init）分离。引导层通过 `BootInfo` 和 `kiface` 单实现入口把主核/从核控制权交给运行时；`kruntime` 提供这些入口的实现（`rust_main` / `rust_main_secondary`）。

启动拓扑遵循 FreeBSD `proc0` 模型：boot/current task 是调度器内部任务（`Internal` 身份，不进入普通 PID allocator）。`rust_main` 激活一个 PID-less 的 late-init 内核线程跑完所有 late init（drivers/fs/net/SMP 等），等待全部 CPU 就绪后调用 `SystemInitEntry::enter()`。`entry` **spawn 出一个全新的 PID 1 用户任务**（init），随后 late-init 线程退出。init 不是"原地变身"——它是被 spawn 的新任务，走与 fork 相同的 `new_user` + `publish_user_task().commit()` 标准路径。

关键设计：late-init 线程以及 late init 期间 spawn 的所有后台 worker（网络 poller、TTY、watchdog 等）都是 PID-less 的 `Internal` 内核线程（对应 FreeBSD `kthread_add`），不进入普通 PID 空间。因此 init 的分配是 root namespace 的第一笔普通分配，PID 1 是唯一的启动期固定 PID。

应用逻辑（例如 `entry::runtime::init_runtime`）放在 `entry` crate；初始用户进程组装则由 `posix-process::spawn_init_process` 承接，避免 `kruntime` 反向依赖高层服务。

## 范围

涉及的源文件：

```
core/kruntime/
├── src/
│   ├── lib.rs              # 主核 rust_main、日志适配、中断/分配器、内存区日志
│   ├── mp.rs               # SMP 从核启动、TLB 单测
│   ├── init_setup.rs       # .init_array 构造函数表遍历
│   ├── dma_integration.rs  # kdma::DmaPageTableIf 实现
│   └── lang_items.rs       # #[panic_handler]
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```
kernel-boot (汇编 / MMU / BootInfo)
        │
        │  PrimaryKernelEntry::enter(boot_info_ptr)
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main (主核, boot task)                         │
│    khal / memspace / kalloc / klogger / backtrace             │
│    init_scheduler / init_interrupt                            │
│    create PID-less late-init bootstrap thread                 │
│    activate late-init, then park PID 0 boot task              │
└───────────────────────────────────────────────────────────────┘
        │
        ▼
┌───────────────────────────────────────────────────────────────┐
│  late_init (Internal, PID-less bootstrap thread)              │
│    drivers / fs / net / SMP / IPI / init_cb                  │
│    等待 INITED_CPUS == nr_cpus                                │
│    SystemInitEntry::enter() ─────────────► entry crate        │
│    entry spawns and activates fresh PID 1 init task           │
│    return, then exit                                           │
└───────────────────────────────────────────────────────────────┘
        │
        │  boot_ap → SecondaryKernelEntry::enter(logical_cpu_id)
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main_secondary (从核)                          │
│    percpu / trap / memspace 从核 / 调度器从核 / IPI            │
│    等待 is_init_ok() → run_idle()                             │
└───────────────────────────────────────────────────────────────┘

侧向接线（链接期，非启动顺序）：
  dma_integration  ──provide──►  kdma::DmaPageTableIf  ──►  memspace
  lib.rs            ──provide──►  klogger::LoggerAdapter
  lib.rs            ──provide──►  kernel_boot::PrimaryKernelEntry
  mp.rs             ──provide──►  kernel_boot::SecondaryKernelEntry
```

| 组件 | 职责 |
|------|------|
| `rust_main` | 主核唯一完整初始化路径；由 `PrimaryKernelEntry` provider 进入 |
| `rust_main_secondary` | 从核初始化；由 `SecondaryKernelEntry` provider 进入；最终 `run_idle` |
| `mp::start_secondary_cpus` | 为 AP 准备栈、调用 `boot_ap`、等待 AP 进入 runtime |
| `init_setup` | 执行链接段 `.init_array` 中的 `register_init` 回调 |
| `dma_integration` | 将 DMA 页表属性更新委托给 `memspace::kernel_layout` |
| `lang_items` | Panic 时打印 backtrace 并 `shutdown` |

## 启动流程（主核）

`rust_main` 阶段划分如下（与代码顺序一致）：

```
BootInfo*
    → firmware::init / percpu::init_primary / init_trap
    → mem::init
    → init_allocator (kalloc 全局堆)
    → register_boot_console_runtime_region (可选 MMIO UART)
    → memspace::init_memory_management
    → early_driver_init
    → klogger + backtrace (链接符号 _stext/_etext)
    → final_init
    → init_scheduler
    → init_interrupt (定时器 tick、IPI、PMU 基础 handler)
    → create late-init bootstrap thread (Internal, PID-less)
    → activate late-init thread
    → boot task (PID 0) block forever

late-init thread (Internal, PID-less):
    → [smp] start_secondary_cpus
    → init_drivers
    → [char/display/input] console handoff / fb / input
    → [fs] fs_boot (root namespace, virtual filesystems)
    → [fs9p] fs_boot host-share mount
    → [net/vsock] knet
    → [ipi] kipi::init, mark_all_cpus_started
    → [watchdog] init_primary
    → init_setup::init_cb
    → finish_allocator_init, log_memory_regions
    → INITED_CPUS += 1
    → spin until is_init_ok()
    → [vsock_tipc_bridge] start_vsock_bridge (依赖跨 CPU 调度，必须在屏障后)
    → SystemInitEntry::enter()
        (entry; spawn 出全新 PID 1 用户任务 init，返回)
    → late-init thread exits (task_entry 自动调 ktask::exit)

init (PID 1, 全新 spawn 的用户任务):
    → 调度器 switch-in 时自动激活用户页表 (switch_page_table_root)
    → run_user_thread_loop → 进入用户态
```

### PID 1 启动约定

`ktask::init_scheduler()` 创建的 boot/current task 与 idle/gc task 都是 scheduler 内部身份（`Internal`/`Idle`），不调用 `kidentity::allocate_root_pid_handle()`。late-init 线程也是 `Internal`，且 late init 期间 spawn 的所有后台 worker（`ktask::spawn` 走 `new_pidless_kthread`）同样是 PID-less 的 `Internal`。因此 `SystemInitEntry::enter()` 创建 init 时，root namespace 的第一笔普通 PID 分配必须得到 PID 1。

这个顺序把 root namespace 的早期 PID 语义固定为：

```text
PID 0   = boot/idle/swapper 类内部任务（不经普通 allocator）
PID 1   = init 用户进程（late-init 线程 spawn 的全新用户任务）
PID >=2 = 后续 fork 出的用户进程或显式 Linux-visible task；普通内核 worker 不占号
```

init 不再经历"原地变身"：late-init 线程通过 `SystemInitEntry::enter()` → `posix-process::spawn_init_process` 构造一个全新的 `User` 身份任务，runtime 在 `new_user` 构造时一次性就绪（`UserRuntimeSlot::ready`），经 `publish_user_task().commit()` 发布并激活，页表由调度器在首次 switch-in 时通过 `switch_page_table_root` 自动写入。没有 `install_user_runtime`、空槽状态机或 `activate_current_user_page_table` 这类"事后补装"机制。

### 全局就绪屏障 `is_init_ok`

```rust
INITED_CPUS.load(Acquire) == kcpu_id_map::nr_cpus()
```

- 主核在完成自身初始化后对 `INITED_CPUS` 执行 `fetch_add(1, Release)`。
- 每个从核在 `rust_main_secondary` 末尾同样 `fetch_add(1, Release)`。
- PID-less late-init 线程在调用 `SystemInitEntry::enter()` 前自旋等待计数达到 `nr_cpus()`（运行时从设备树/ACPI 发现的实际核数，而非编译期 `NR_CPUS` 上限），保证系统初始化入口启动时所有逻辑 CPU 已完成 runtime 初始化。QEMU `-smp` 与 `NR_CPUS` 不再强耦合：给少于 `NR_CPUS` 的核不会死锁，给多于 `NR_CPUS` 的核会被告警截断。
- 依赖跨 CPU task spawn 的 late-start worker，例如 vsock-TIPC bridge，
  在该屏障之后、`SystemInitEntry::enter()` 之前启动，避免调度到尚未注册的 secondary run queue。

从核在屏障之后开启本地 IRQ（及可选 watchdog），进入 `ktask::run_idle()`，**不**执行 `SystemInitEntry`。

## 启动流程（从核）

```
SecondaryKernelEntry::enter(logical_cpu_id)
    → percpu::init_secondary, init_trap
    → ENTERED_CPUS += 1  (供 start_secondary_cpus 等待)
    → memspace::init_memory_management_secondary
    → final_init_secondary, init_scheduler_secondary
    → [ipi] kipi::init
    → INITED_CPUS += 1
    → spin until is_init_ok()
    → enable IPI/PMU IRQ, enable_local_irq
    → [watchdog] init_secondary
    → run_idle()  (永不返回)
```

`start_secondary_cpus` 与从核通过 `ENTERED_CPUS` 握手：主核每启动一个 AP，等待其 `ENTERED_CPUS` 递增后再启动下一个，避免栈或未映射内存被并发使用。

## `.init_array` 与 `register_init`

`init_setup.rs` 维护链接器段 `.init_array`：

- `_SECTION_PLACE_HOLDER`：保证空镜像也有 `__init_array_start` / `__init_array_end`。
- `init_cb()`：在 `init_interrupt` 之后、`finish_allocator_init` 之前，按指针步进调用段内每个 `extern "C" fn()`。

各子系统可通过 `util/macros` 的 `#[register_init]` 向该段注册早期 init，无需修改 `kruntime` 源码列表。

## `kiface` 接线

| 接口 | 实现位置 | 目的 |
|------|----------|------|
| `kdma::DmaPageTableIf` | `dma_integration.rs` | 打破 `kdma` ↔ `memspace` 循环依赖 |
| `klogger::LoggerAdapter` | `lib.rs` | 日志输出到 `khal::console`，附带 CPU/任务 ID |
| `kernel_boot::PrimaryKernelEntry` | `lib.rs` | 主核从 boot 层进入 `rust_main` |
| `kernel_boot::SecondaryKernelEntry` | `mp.rs` | 从核从 boot 层进入 `rust_main_secondary` |
| `fs_block::FileSystemType` | Kconfig 所选 `kext4_vfs` / `fat` crate | 提供 root block filesystem mount，避免 boot 按实现分支 |
| `kruntime::SystemInitEntry` | `entry/src/main.rs` | runtime 就绪后进入系统级 init 策略层 |
均为链接期 exactly-one 单实现，非运行时注册。

## Cargo Features

| Feature | 作用 |
|---------|------|
| `smp` | 从核启动、`rust_main_secondary`、`memspace`/`ktask`/`khal` SMP |
| `ipi` | 依赖 `kipi`；IPI 中断处理 |
| `fs` / `fs9p` / `net` / `vsock` / `display` / `input` | 驱动与子系统初始化（经 `kdriver`）；具体 root filesystem feature 只链接对应 `FileSystemType` provider |
| `rtc` | 启动时打印墙钟时间 |
| `watchdog` / `watchdog_hardlockup` | 看门狗主/从核初始化 |
| `pmu` | PMU 溢出中断 |
| `arm-timer-resume-fixup` | 修复 AArch64 虚拟计时器在 idle/WFI 返回后的计数回退 |
| `rootfs-secondary-block` | 将第二个块设备作为根文件系统后端 |

默认全部关闭；由 `entry` / `kfeat` 的 defconfig 打开。

## 设计决策

### 为何用 `SystemInitEntry` 而非直接依赖 `entry`

`kruntime` 被 `entry` 依赖。若 `kruntime` 再依赖 `entry` 会形成环。`SystemInitEntry` 把 handoff 变成一个 `kiface` 单实现接口：`kruntime` 拥有调用契约，`entry` 提供策略实现，同时避免依赖裸符号名和 `extern "C"` 调用。

### 为何主核才跑 `init_cb`

`.init_array` 回调假定中断子系统已注册、且尚未进入多任务应用阶段；放在 `init_interrupt` 之后、调用 `SystemInitEntry::enter()` 之前，与 C 运行时 constructor 时机相近，但由内核显式控制调用点。

### 为何 DMA 接线放在独立模块

保持 `lib.rs` 聚焦启动编排；`dma_integration` 仅一行业务逻辑，便于审查与替换实现。

### 日志适配放在 `lib.rs`

`LoggerAdapter` 需在 `klogger::init_klogger` 之前通过 `kiface` provider 链接生效；与 `rust_main` 同文件可避免初始化顺序问题。

## 与 QEMU / 真机的边界

`kruntime` 本身**平台无关**：`BootInfo` 内容由 `kernel-boot` + 平台 crate 填充。QEMU virt 与板级（如 raspi）差异在引导层和 `khal::final_init`，进入 `rust_main` 后的阶段表相同。

## 冗余与过载

- **冗余设计**：无（单路径主核 init）。
- **过载控制**：不在此 crate；调度与内存压力由 `ktask` / `kalloc` 负责。
