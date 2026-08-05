# khal::irq - 安全与可靠性分析

## 信任模型

`khal::irq` 是 trap-entry adapter，不拥有 generic IRQ state，也不 re-export
generic `kirq` API。
它信任：

- `kcpu` trap dispatch 只在正确异常上下文调用 `irq_handler()` / `nmi_handler()`；
- `kspin::NoPreempt` 能覆盖进入 `kirq` 的 adapter 调用区间；
- `kirq` 维护 IRQ state、NMI table、dispatch 和 completion 不变量。

## 外部边界 / 攻击面

1. **trap dispatch 边界**
   `kcpu` 通过 registered trap handler 进入本模块。错误 trap 分类会把 normal IRQ
   与 pseudo-NMI 语义混淆。

本模块不直接处理用户指针、DMA buffer、设备 MMIO 或文件/网络数据。

`arch/khal/src/irq` 当前没有本地 `unsafe` 代码块。相关 unsafe 边界位于：

- `kcpu` trap/汇编入口；
- 平台中断控制器 backend 的 MMIO/PIO/汇编屏障；
- `kiface` generated linkage。

## 内存安全不变量

- `khal::irq` 不保存 `IRQ_STATE` 或 `NMI_TABLE`，避免与 `kirq` 形成重复活跃 state。
- `NoPreempt` guard 必须覆盖整个 `kirq::handle_irq()` / `kirq::handle_nmi()` 调用。

## 线程安全

`khal::irq` adapter 不引入新的共享 mutable state。并发控制主要由：

- `NoPreempt` 保护 trap adapter 执行区间；
- `kirq` 内部 lock 模型保护 IRQ core state；
- `kiface` 单实现接口连接 platform/backend provider。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | trap adapter 未禁用抢占 | handler 直接调用 `kirq` 而无 `NoPreempt` | hardirq exit 后调度边界错误 | `irq_handler()` / `nmi_handler()` 创建 `NoPreempt` |
| T-02 | generic IRQ API 重新经由 `khal::irq` 导出 | 调用方把 HAL adapter 当公共 IRQ core | crate 边界退化，后续扩展回到 HAL | `khal::irq` 不 re-export `kirq::*`，调用方直接依赖 `kirq` |
| T-03 | `IPI_IRQ` 移入 `kirq` | generic core 依赖 `kbuild_config` | crate 边界污染、特性耦合 | IPI 调用方直接使用 `kbuild_config::IPI_IRQ` |
| T-04 | x86 helper 回流到 `khal::irq` | HAL adapter 再次暴露 APIC vector policy | crate 边界退化，驱动绕过 `kirq` MSI resource | `khal::irq` 不提供 MSI-X/APIC helper；x86 APIC 只实现 `kirq::MsiBackendIf` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | `kirq::handle_irq()` 返回前 `NoPreempt` 过早释放 | lifecycle/dispatch 上下文错误 | 调度状态异常 | adapter guard 生命周期覆盖整个调用 |
| F-02 | 调用方仍使用 `khal::irq` generic API | 编译失败 | 暴露未迁移调用点 | `rg "khal::irq"` audit + Make/Kconfig build |
| F-03 | trap handler 未链接 | CPU IRQ/NMI 无法进入 `kirq` | 平台无法处理中断 | platform build 覆盖和 trap registration 检查 |

## 故障管理

`khal::irq` adapter 本身不处理 recoverable IRQ 错误；`kirq` 负责注册失败、
unknown IRQ/NMI warning 和 completion 管理。adapter 返回 `kirq` handler 的布尔结果。

## 隐私分析

`khal::irq` 不处理用户数据。它只处理 trap vector。

## 已知限制

- 本模块文档只覆盖 adapter；IRQ core 威胁模型见 `arch/kirq/docs/security.md`。

## 审计清单

- `khal::irq` 是否仍只作为 adapter/facade，不新增 IRQ state。
- trap handler 是否在调用 `kirq` 前创建 `NoPreempt`。
- `khal::irq` 是否没有重新导出 generic `kirq` API。
- x86 APIC helper 是否仍未回流到 `khal::irq`。
