# device-res — 设计文档

## 定位

`device-res` 提供 OS 无关的设备资源描述模型和 provider contract。它是驱动
子系统表达“驱动需要哪些内核能力”的统一入口，覆盖 MMIO 映射、I/O 端口、
中断、DMA 缓冲区和单调时钟等能力，并将这些语义与具体内核实现解耦。

依赖本模块的上游子系统包括：

- 各平台驱动（通过 RAII handle 或 `devm_*` 函数获取资源）
- 内核总线/设备模型（实现 [`DeviceResource`] trait 以支持设备托管资源）

## 背景

不同内核对同一类能力（映射 MMIO、注册中断、分配 DMA、读取单调时间）的操作接口各异。
驱动代码如果直接调用内核 API，移植时需要逐函数修改。本模块将资源的发现与使用
分离：驱动只描述"需要什么"，由 host kernel 通过 MMIO / IRQ / DMA provider traits
提供"怎么给"，从而实现驱动在不同内核间的可移植性。

## 范围

涉及的源文件：

```
drivers/contracts/device-res/
├── src/
│   ├── lib.rs
│   ├── dma.rs
│   ├── irq.rs
│   ├── mmio.rs
│   ├── provider.rs
│   └── time.rs
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```
                    ┌─────────────────────────────────────────────┐
                    │             Host Kernel                      │
                    │  implements provider traits                  │
                    │  passes provider to driver framework         │
                    └───────────────┬─────────────────────────────┘
                                    │
            ┌───────────────────────┼───────────────────────┐
            │                       │                       │
            v                       v                       v
     ┌──────────────┐      ┌──────────────┐      ┌──────────────────┐
     │  Io (MMIO)   │      │  Irq         │      │  DmaCoherent     │
     │  RAII handle │      │  RAII handle │      │  RAII handle     │
     │  map on new  │      │  request     │      │  alloc on new    │
     │  unmap drop  │      │  release drop│      │  free on drop    │
     └──────────────┘      └──────────────┘      └──────────────────┘
            ^                       ^                       ^
            │                       │                       │
     ┌─────────────────────────────────────────────────────────────┐
     │              devm_* helpers (device-managed)                │
     │  devm_iomap / devm_request_irq* / devm_alloc_coherent      │
     │  register cleanup via DeviceResource trait                 │
     └─────────────────────────────────────────────────────────────┘
                        ^
                        │
               ┌────────────────┐
               │  Driver Code   │
               │  reads/writes  │
               │  registers via │
               │  RAII or devm  │
               └────────────────┘
```

### 核心组件

| 组件 | 职责 |
|------|------|
| `ResourceDesc` / `ResourceSet` | 描述单个设备的硬件资源（MMIO、I/O 端口、中断、DMA） |
| `MmioOp` / `IrqOp` / `DmaOp` / `TimeOp` traits | host kernel 实现的能力后端（map/unmap、request/release IRQ、alloc/free DMA、alloc/free MSI-X、monotonic time） |
| `ResourceProvider` trait | `MmioOp + IrqOp + DmaOp + TimeOp` 的组合 trait，供驱动框架持有完整资源能力 |
| `Io` | MMIO 映射的 RAII handle，提供带 acquire/release fence 的寄存器读写方法 |
| `Irq` | 中断注册的 RAII handle，drop 时自动释放 |
| `DmaCoherent` | 一致性 DMA 缓冲区的 RAII handle，drop 时自动释放 |
| `DeviceResource` trait | OS 无关的设备抽象，驱动通过它读取资源和注册清理回调 |
| `devm_*_with_provider` 函数 | 将资源生命周期绑定到设备，probe 失败或移除时自动清理 |

### Provider 选择

`device-res` 不维护全局 provider 状态。驱动框架持有 host kernel 提供的
provider 实例，并调用
`Io::map_with()`、`Irq::request_with()`、`DmaCoherent::alloc_with()` 或
`devm_*_with_provider()`。RAII handle 保存创建它的 provider，drop 时回到同一个
provider 执行释放。

## 调用约束 / 执行上下文

- **可在早期启动阶段调用**：模块不依赖调度器或进程线程上下文，
  provider 生命周期由调用方保证。
- **不可在中断上下文中获取或释放资源**：provider trait 方法文档声明运行在
  正常（非中断）上下文。MMIO 读写方法本身可以在任意上下文调用，
  但资源获取/释放（`map`、`request`、`alloc` 及对应的 drop）不应
  在中断上下文中执行。`TimeOp::monotonic_time()` 可用于短轮询和超时检查，
  provider 实现必须声明自身是否可在 IRQ-like 上下文调用。
- **不可睡眠或阻塞**：`device-res` 本身不获取全局 provider 锁；provider 方法
  自身仍必须遵守各 host kernel 的 probe/remove 上下文约束。
- **不要求当前进程线程**：API 只依赖当前执行路径。
- **可重入性**：`device-res` 不持全局 provider 锁调用 provider；provider 实现仍应
  避免在资源释放回调中形成自身的锁递归。

## 算法流程

### 资源获取（以 `Io::map_with` 为例）

```
Io::map_with(provider, region, name)
  │
  ├─ provider.map_mmio(region, name)?
  │    └─ host kernel 执行实际映射
  │
  └─ Ok(Io { provider, mapping: Some(mapping) })
```

### RAII 资源释放（以 `Io::drop` 为例）

```
Io::drop()
  │
  ├─ mapping.take()
  ├─ provider.take()
  │
  └─ if both Some:
       provider.unmap_mmio(mapping)
```

RAII handle 保存创建它的 provider。drop 不重新查询外部状态，因此不会把释放
请求发送给另一个 provider，也不依赖框架在 cleanup 时重新选择 provider。

### 设备托管资源（以 `devm_iomap` 为例）

```
devm_iomap_with_provider(provider, device, region, name)
  │
  ├─ Io::map_with(provider, region, name)?  → io
  ├─ io.as_ptr()                           → ptr
  ├─ device.register_cleanup(move || drop(io))
  │    └─ 回调在设备移除时 LIFO 执行
  │
  └─ Ok(ptr)
```

## 并发模型

- **显式 provider**：驱动框架持有 provider，资源 handle 只保存 `&'static dyn ...`
  引用，不引入额外共享可变状态。
- **RAII handle**：`Io`、`Irq`、`DmaCoherent` 均非 `Sync`（内部持有
  `NonNull`），不可跨线程共享。它们可在线程间移动（`Send`），
  但同一时刻只有一个线程持有 handle。
- **MMIO 读写**：`Io` 的 `read*`/`write*` 方法使用 acquire/release
  fence 保证寄存器访问的有序性。多字节访问有 `debug_assert` 检查对齐。

## 设计决策

### 为什么 provider API 用 trait 对象

RAII handle 内部保存 `&'static dyn MmioOp` / `IrqOp` / `DmaOp`，原因：

- 驱动框架可以持有具体 provider 类型并通过 trait bound 管理能力；
- handle 需要在 devres cleanup 闭包中保存 provider 引用，trait object 能避免把
  provider 泛型扩散到普通驱动和 cleanup 容器；
- provider 选择在框架边界显式完成，驱动代码仍只看到设备资源方法。

### 为什么 `devm_*` 返回裸指针而非 RAII handle

`devm_iomap` 返回 `NonNull<u8>` 而非 `Io`，因为 `Io` 的 drop 行为
与设备托管清理冲突：如果返回 `Io`，驱动 drop `Io` 时会释放映射，
同时设备清理回调也会尝试释放同一映射。

解决方案：`devm_iomap` 在内部创建 `Io`，提取指针，然后通过
`register_cleanup` 注册 drop 闭包。`Io` 的生命周期由回调管理，
驱动只持有裸指针。

### 为什么 `Io::read*`/`write*` 使用 fence 而非 `volatile` 的 `Ordering` 参数

`core::ptr::read_volatile` / `write_volatile` 保证编译器不会消除或重排
volatile 访问，但不提供 CPU 侧的内存序保证。额外的 `fence(Acquire)` /
`fence(Release)` 确保在弱序架构（AArch64）上，寄存器读写不会被 CPU
重排到 fence 另一侧。在强序架构（x86）上 fence 编译为空操作。

### 为什么 drop 保存创建时的 provider

资源释放必须回到分配该资源的同一个 provider。否则未来出现 per-framework、
per-bus 或测试 mock provider 时，drop 阶段重新选择 provider 可能释放到错误后端。
保存创建时 provider 可以把 acquire/release 配对关系编码进 RAII handle。

## Drop / 资源释放

| 类型 | Drop 行为 |
|------|----------|
| `Io` | 如果 mapping 和 provider 均为 `Some`，调用 `provider.unmap_mmio(mapping)` |
| `Irq` | 如果 `armed` 且 provider 为 `Some`，调用 `provider.release_irq(resource, token)` |
| `DmaCoherent` | 如果 allocation 和 provider 均为 `Some`，调用 `provider.free_coherent(allocation)` |

`Irq` 使用 `armed` 标志防止 `request_irq` 失败后 drop 时误调用 `release_irq`，
并保存 provider 返回的 token，使共享 IRQ 释放时只移除当前注册的 handler。

### MSI-X 资源

`IrqOp::alloc_msix()` 返回 `MsiResource`：

- `MsiResource::irq` 是 OS-visible IRQ，驱动或上层框架用它注册 handler；
- `MsiResource::message` 是 device-visible MSI message，PCI/MSI-X 代码用它写设备
  table 或 MSI register。

`MsiResource` 是显式所有权资源，不实现 `Copy`。调用方可以在同一所有权链路中移动
它，并必须在 IRQ handler 注销之后调用 `free_msix()` 释放；不能通过隐式复制制造多个
同一 MSI allocation 的释放者。

`device-res` 不暴露 APIC id、CPU vector 或 irqchip-private allocation cookie。
这些属于 host IRQ core 和具体 backend 的内部状态。x-kernel 中该 provider 由
`device-res-xkernel` 实现，内部转到 `kirq::alloc_msix()`；`device-res` 本身仍保持
OS-neutral，不依赖 `kirq`。
