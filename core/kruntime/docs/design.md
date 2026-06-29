# kruntime — 设计文档

## 定位

`kruntime` 是 x-kernel 的**运行时编排层**：在平台引导代码将控制权交给内核后，负责主核/从核的初始化顺序、子系统装配，以及跳转到应用入口（`entry` crate 的 `main`）。它不实现具体设备驱动或 syscall，而是把 `khal`、`memspace`、`ktask`、`kdriver` 等按固定阶段串联起来。

目标读者：需要理解启动链、SMP 屏障、或在此 crate 增加初始化钩子的内核开发者。

## 背景

x-kernel 将**引导**（`kernel-boot`、平台 `platconfig`）与**运行时**（内存、调度、驱动、用户态 init）分离。引导层通过 `BootInfo` 和 `linkme` 切片把主核/从核入口注册为函数指针；`kruntime` 提供这些入口的实现（`rust_main` / `rust_main_secondary`），并在全部 CPU 就绪后调用链接进来的 `main()`。

应用逻辑（例如 `entry::runtime::init_runtime`）放在 `entry` crate；初始用户进程组装则由 `posix-process::run_init_process` 承接，避免 `kruntime` 反向依赖高层服务。

## 范围

涉及的源文件：

```
core/kruntime/
├── src/
│   ├── lib.rs              # 主核 rust_main、日志适配、中断/分配器、内存区日志
│   ├── mp.rs               # SMP 从核启动、TaskCpuResidencyIf、TLB 单测
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
        │  call_kernel_entry!(PRIMARY_KERNEL_ENTRY, boot_info_ptr)
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main (主核)                                    │
│    khal / memspace / kalloc / klogger / backtrace             │
│    ktask / kdriver·kfs·knet (feature) / SMP / IPI / IRQ        │
│    init_setup::init_cb (.init_array)                          │
│    等待 INITED_CPUS == CPU_NUM                                 │
│    main()  ──────────────────────────────►  entry crate       │
└───────────────────────────────────────────────────────────────┘
        │
        │  boot_ap → SECOND_KERNEL_ENTRY
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main_secondary (从核)                          │
│    percpu / trap / memspace 从核 / 调度器从核 / IPI            │
│    等待 is_init_ok() → run_idle()                             │
└───────────────────────────────────────────────────────────────┘

侧向接线（链接期，非启动顺序）：
  dma_integration  ──impl──►  kdma::DmaPageTableIf  ──►  memspace
  LogIfImpl          ──impl──►  klogger::LoggerAdapter
  TaskCpuResidencyImpl ──impl──► kipi::tlb::TaskCpuResidencyIf
```

| 组件 | 职责 |
|------|------|
| `rust_main` | 主核唯一完整初始化路径；注册为 `PRIMARY_KERNEL_ENTRY` |
| `rust_main_secondary` | 从核初始化；注册为 `SECOND_KERNEL_ENTRY`；最终 `run_idle` |
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
    → [feature] init_drivers → kfs / knet / fb / input
    → [smp] start_secondary_cpus
    → [ipi] kipi::init, mark_all_cpus_started
    → init_interrupt (定时器 tick、IPI、PMU)
    → [watchdog] init_primary
    → init_setup::init_cb
    → finish_allocator_init, log_memory_regions
    → INITED_CPUS += 1
    → spin until is_init_ok()
    → main()  (entry)
    → ktask::exit(0)
```

### 全局就绪屏障 `is_init_ok`

```rust
INITED_CPUS.load(Acquire) == kbuild_config::CPU_NUM
```

- 主核在完成自身初始化后对 `INITED_CPUS` 执行 `fetch_add(1, Release)`。
- 每个从核在 `rust_main_secondary` 末尾同样 `fetch_add(1, Release)`。
- 主核在调用 `main()` 前自旋等待计数达到 `CPU_NUM`，保证应用入口启动时所有逻辑 CPU 已完成 runtime 初始化。

从核在屏障之后开启本地 IRQ（及可选 watchdog），进入 `ktask::run_idle()`，**不**执行 `main()`。

## 启动流程（从核）

```
SECOND_KERNEL_ENTRY(logical_cpu_id)
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

## `crate_interface` 接线

| 接口 | 实现位置 | 目的 |
|------|----------|------|
| `kdma::DmaPageTableIf` | `dma_integration.rs` | 打破 `kdma` ↔ `memspace` 循环依赖 |
| `klogger::LoggerAdapter` | `lib.rs` `LogIfImpl` | 日志输出到 `khal::console`，附带 CPU/任务 ID |
| `kipi::tlb::TaskCpuResidencyIf` | `mp.rs` | TLB shootdown 查询任务 CPU 驻留掩码 |

均为链接期单实现，非运行时注册。

## Cargo Features

| Feature | 作用 |
|---------|------|
| `smp` | 从核启动、`rust_main_secondary`、`memspace`/`ktask`/`khal` SMP |
| `ipi` | 依赖 `kipi`；IPI 中断处理 |
| `fs` / `fs9p` / `net` / `vsock` / `display` / `input` | 驱动与子系统初始化（经 `kdriver`） |
| `rtc` | 启动时打印墙钟时间 |
| `watchdog` / `watchdog_hardlockup` | 看门狗主/从核初始化 |
| `pmu` | PMU 溢出中断 |
| `arm-timer-resume-fixup` | 修复 AArch64 虚拟计时器在 idle/WFI 返回后的计数回退 |
| `rootfs-secondary-block` | 将第二个块设备作为根文件系统后端 |

默认全部关闭；由 `entry` / `kfeat` 的 defconfig 打开。

## 设计决策

### 为何 `main` 是 `extern "C"` 而非直接依赖 `entry`

`kruntime` 被 `entry` 依赖。若 `kruntime` 再依赖 `entry` 会形成环。应用入口以符号 `main` 由链接器解析，`entry` 提供 `#[no_mangle] fn main()`。

### 为何主核才跑 `init_cb`

`.init_array` 回调假定中断子系统已注册、且尚未进入多任务应用阶段；放在 `init_interrupt` 之后、调用 `main()` 之前，与 C 运行时 constructor 时机相近，但由内核显式控制调用点。

### 为何 DMA 接线放在独立模块

保持 `lib.rs` 聚焦启动编排；`dma_integration` 仅一行业务逻辑，便于审查与替换实现。

### 日志适配放在 `lib.rs`

`LoggerAdapter` 需在 `klogger::init_klogger` 之前通过 `impl_interface` 链接生效；与 `rust_main` 同文件可避免初始化顺序问题。

## 与 QEMU / 真机的边界

`kruntime` 本身**平台无关**：`BootInfo` 内容由 `kernel-boot` + 平台 crate 填充。QEMU virt 与板级（如 raspi）差异在引导层和 `khal::final_init`，进入 `rust_main` 后的阶段表相同。

## 冗余与过载

- **冗余设计**：无（单路径主核 init）。
- **过载控制**：不在此 crate；调度与内存压力由 `ktask` / `kalloc` 负责。
