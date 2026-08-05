# khal::irq - 设计文档

## 定位

`khal::irq` 是 IRQ trap-entry adapter。通用 IRQ core
位于 `kirq`；外部调用方不应再通过 `khal::irq::*` 使用 generic IRQ API。

本模块现在只拥有 trap-entry 相关职责：

- 注册 `IRQ` / `NMI` trap handler；
- 在进入 `kirq` 前建立 `NoPreempt` 执行约束。

## 范围

```text
arch/khal/src/irq/
├── mod.rs       # adapter helper exports only
└── manager.rs   # trap handlers
```

generic IRQ state、descriptor、dispatch、NMI table、lifecycle hooks 和
`IntrManagerIf` contract 由 `arch/kirq` 文档描述。

## 架构

```text
kcpu trap dispatch
    |
    `-- khal::irq
            |-- #[register_trap_handler(IRQ)]
            |-- #[register_trap_handler(NMI)]
            |-- NoPreempt guard
            `-- kirq::{handle_irq, handle_nmi}
```

普通 IRQ API 调用路径应直接进入 `kirq`，不经过 `khal::irq` facade。

## 调用约束 / 执行上下文

- `irq_handler()` 与 `nmi_handler()` 只能由架构 trap dispatch 调用。
- 两个 handler 都先创建 `kspin::NoPreempt`，再调用 `kirq`。
- `irq_handler()` 调用 `kirq::handle_irq()`；hardirq lifecycle enter/exit 和
  IRQ completion 顺序由 `kirq` 负责。
- `nmi_handler()` 调用 `kirq::handle_nmi()`；NMI raw-hwirq dispatch 语义由
  `kirq` 负责。
- `khal::irq` 自身不保存 `IRQ_STATE` 或 `NMI_TABLE`，避免两个活跃 IRQ core。

## 并发模型

`khal::irq` adapter 只有短生命周期的 `NoPreempt` guard，不持有全局 IRQ state。
IRQ shared state、NMI table 和 lifecycle hooks 的锁模型都在 `kirq` 内部维护。

## 设计决策

### 为什么保留 `khal::irq`

trap handler registration 依赖 `kcpu::excp::register_trap_handler`，并且进入
`kirq` 前需要由 HAL adapter 建立 `NoPreempt` 执行约束。因此 `khal::irq`
继续存在，但不再承载 generic IRQ API 或 x86 MSI-X vector helper。

### 为什么 x86 MSI-X helper 不在 `khal::irq`

MSI-X vector allocation、APIC destination selection 和 MSI message composition
属于 IRQ core/backend 边界。当前由 `kirq::MsiBackendIf` 定义 backend contract，
由 `drivers/x86-apic` 实现；`khal::irq` 不再暴露 APIC id 或裸 CPU vector。

### 为什么 `IPI_IRQ` 不进入 `kirq`

`IPI_IRQ` 来自 Kconfig 生成常量。`kirq` 必须保持不依赖 `kbuild_config`，所以
IPI 调用方直接使用 `kbuild_config::IPI_IRQ` 并调用 `kirq::notify_cpu()`。
