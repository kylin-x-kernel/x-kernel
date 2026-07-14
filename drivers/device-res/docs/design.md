# device-res — 设计文档

## 定位

`device-res` 提供 OS 无关的设备资源描述模型和提供者抽象层。它是 x-kernel
中驱动程序获取硬件资源（MMIO 映射、I/O 端口、中断、DMA 缓冲区）的统一入口，
将资源语义与具体内核实现解耦。

依赖本模块的上游子系统包括：

- 各平台驱动（通过 RAII handle 或 `devm_*` 函数获取资源）
- 内核总线/设备模型（实现 [`DeviceResource`] trait 以支持设备托管资源）

## 背景

不同内核对同一类硬件资源（映射 MMIO、注册中断、分配 DMA）的操作接口各异。
驱动代码如果直接调用内核 API，移植时需要逐函数修改。本模块将资源的发现与使用
分离：驱动只描述"需要什么"，由 host kernel 通过 [`ResourceProvider`] trait
提供"怎么给"，从而实现驱动在不同内核间的可移植性。

## 范围

涉及的源文件：

```
drivers/device-res/
├── src/
│   └── lib.rs
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```
                    ┌─────────────────────────────────────────────┐
                    │             Host Kernel                      │
                    │  implements ResourceProvider trait           │
                    │  calls set_provider() during early init     │
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
     │  devm_iomap / devm_request_irq / devm_alloc_coherent       │
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
| `ResourceProvider` trait | host kernel 实现的资源操作后端（map/unmap、request/release IRQ、alloc/free DMA） |
| `Io` | MMIO 映射的 RAII handle，提供带 acquire/release fence 的寄存器读写方法 |
| `Irq` | 中断注册的 RAII handle，drop 时自动释放 |
| `DmaCoherent` | 一致性 DMA 缓冲区的 RAII handle，drop 时自动释放 |
| `DeviceResource` trait | OS 无关的设备抽象，驱动通过它读取资源和注册清理回调 |
| `devm_*` 函数 | 将资源生命周期绑定到设备，probe 失败或移除时自动清理 |

### 全局状态

提供者通过 `SpinNoIrq<Option<&'static dyn ResourceProvider>>` 静态存储，
在内核早期初始化时通过 `set_provider()` 安装一次。所有资源获取操作
通过 `provider()` / `try_provider()` 访问该全局实例。

## 调用约束 / 执行上下文

- **可在早期启动阶段调用**：模块不依赖调度器或进程线程上下文，
  仅使用自旋锁保护全局提供者。
- **不可在中断上下文中调用**：`ResourceProvider` 方法文档声明运行在
  正常（非中断）上下文。MMIO 读写方法本身可以在任意上下文调用，
  但资源获取/释放（`map`、`request`、`alloc` 及对应的 drop）不应
  在中断上下文中执行。
- **不可睡眠或阻塞**：全局提供者通过 `SpinNoIrq` 保护，持锁期间
  不可睡眠。
- **不要求当前进程线程**：API 只依赖当前执行路径。
- **可重入性有限**：持自旋锁期间调用其他 `device-res` 函数会导致死锁。

## 算法流程

### 资源获取（以 `Io::map` 为例）

```
Io::map(region, name)
  │
  ├─ provider()
  │    └─ lock(PROVIDER) → ok_or(NoProvider)
  │
  ├─ provider.map_mmio(region, name)?
  │    └─ host kernel 执行实际映射
  │
  └─ Ok(Io { mapping: Some(mapping) })
```

### RAII 资源释放（以 `Io::drop` 为例）

```
Io::drop()
  │
  ├─ mapping.take()
  ├─ try_provider()
  │    └─ lock(PROVIDER) → Option
  │
  └─ if both Some:
       provider.unmap_mmio(mapping)
```

在 drop 中使用 `try_provider()` 而非 `provider()`：如果提供者已被卸载
或系统正在关闭，静默跳过释放而非 panic。

### 设备托管资源（以 `devm_iomap` 为例）

```
devm_iomap(device, region, name)
  │
  ├─ Io::map(region, name)?  → io
  ├─ io.as_ptr()             → ptr
  ├─ device.register_cleanup(move || drop(io))
  │    └─ 回调在设备移除时 LIFO 执行
  │
  └─ Ok(ptr)
```

## 并发模型

- **全局提供者**：`SpinNoIrq` 自旋锁保护，锁持有期间禁止中断。
  所有对提供者的读写（`set_provider`、`provider`、`try_provider`）
  均需持锁。
- **RAII handle**：`Io`、`Irq`、`DmaCoherent` 均非 `Sync`（内部持有
  `NonNull`），不可跨线程共享。它们可在线程间移动（`Send`），
  但同一时刻只有一个线程持有 handle。
- **MMIO 读写**：`Io` 的 `read*`/`write*` 方法使用 acquire/release
  fence 保证寄存器访问的有序性。多字节访问有 `debug_assert` 检查对齐。

## 设计决策

### 为什么用 trait 对象而非泛型

`ResourceProvider` 以 `&'static dyn ResourceProvider` 全局存储，
而非泛型参数。原因：

- 全局静态存储只能存储具体类型，使用 `dyn` 避免为每个调用者传递泛型参数。
- 系统中只有一个提供者实例，泛型参数化没有收益。
- `SpinNoIrq<Option<&'static dyn ResourceProvider>>` 是已知模式，
  运行时开销可接受。

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

### 为什么 drop 用 `try_provider` 而非 `provider`

如果系统关闭顺序导致提供者先于 RAII handle 被卸载，`provider()` 会
返回 `Err(NoProvider)` 并在 `drop` 中 panic。`try_provider()` 返回
`Option`，允许 drop 静默跳过——这在关闭路径中更安全。

## Drop / 资源释放

| 类型 | Drop 行为 |
|------|----------|
| `Io` | 如果 mapping 和 provider 均为 `Some`，调用 `provider.unmap_mmio(mapping)` |
| `Irq` | 如果 `armed` 且 provider 为 `Some`，调用 `provider.release_irq(resource, token)` |
| `DmaCoherent` | 如果 allocation 和 provider 均为 `Some`，调用 `provider.free_coherent(allocation)` |

所有 drop 路径使用 `try_provider()` 避免在系统关闭时 panic。
`Irq` 使用 `armed` 标志防止 `request_irq` 失败后 drop 时误调用 `release_irq`，
并保存 provider 返回的 token，使共享 IRQ 释放时只移除当前注册的 handler。
