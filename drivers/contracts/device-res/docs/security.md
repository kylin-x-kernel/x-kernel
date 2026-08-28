# device-res — 安全与可靠性分析

## 概述

`device-res` 是 x-kernel 中驱动获取硬件资源的统一抽象层。模块直接操作 MMIO
寄存器和 DMA 缓冲区，包含 `unsafe impl Send`、裸指针解引用和 volatile 访问。
不正确使用或不变量破损可能导致未定义行为或设备故障。

## 信任模型

```
驱动代码（trusted）
   │
   │ safe API: Io::map_with, Irq::request_with,
   │           DmaCoherent::alloc_with, devm_*_with_provider
   │
   v
┌──────────────────────────────────────────────────────────────┐
│  device-res                                                  │
│                                                              │
│  ┌── unsafe 边界 ──────────────────────────────────────────┐ │
│  │ unsafe impl Send for MmioMapping, DmaAllocation        │ │
│  │ Io::access_ptr → ptr.add(offset)                       │ │
│  │ Io read*/write* → ptr.read_volatile / write_volatile   │ │
│  └─────────────────────────────────────────────────────────┘ │
│                                                              │
│  MmioOp / IrqOp / DmaOp / TimeOp provider traits            │
└──────────────────┬───────────────────────────────────────────┘
                   │
                   v
         Host Kernel 实现（trusted）
```

- **驱动代码**：信任 `device-res` 正确管理 RAII handle 的生命周期和边界检查。
- **Host Kernel 提供者**：信任 `MmioOp` / `IrqOp` / `DmaOp` / `TimeOp`
  实现正确执行映射、注册和释放操作。
- **驱动框架**：负责持有并显式传入 host provider；`device-res` 不维护全局
  provider 状态。
- **设备固件/硬件**：不可信输入源——MMIO 读值和中断触发来自外部。

## 外部边界 / 攻击面

| 边界 | 类型 | 说明 |
|------|------|------|
| MMIO 寄存器 | 设备输入 | `read_volatile` 读回的值来自硬件，可能是任意值 |
| MMIO 寄存器 | 设备输出 | `write_volatile` 写入设备可见的寄存器 |
| DMA 缓冲区 | 设备可写内存 | 一致性 DMA 缓冲区可被设备并发修改 |
| 中断 | 设备信号 | 中断处理器在中断上下文中被调用，频率由设备决定 |
| MSI message | 设备输出 | `MsiResource::message` 被写入设备 MSI/MSI-X 寄存器或 table |
| Firmware/ACPI 资源描述 | 引导元数据 | `ResourceDesc`（地址、大小、中断号）来自固件发现 |
| 单调时钟 | host 输入 | `TimeOp::monotonic_time()` 来自 host kernel，用于驱动超时判断 |

本模块：

- **不直接访问用户内存**；
- **不直接解析 bootloader/firmware 输入**——资源描述由调用者构造；
- **不依赖 FFI 或内联汇编**——使用 `core::ptr` volatile 操作；
- **不处理文件系统、网络或 IPC 外部输入**。

## unsafe 代码清单

### 1. `unsafe impl Send for MmioMapping`（`src/lib.rs`）

```rust
unsafe impl Send for MmioMapping {}
```

**不变量**：`MmioMapping` 仅携带地址值和普通描述符。底层设备映射的所有权
由单个 `Io` handle 独占，地址在映射生命周期内有效，释放回到创建该 handle 的
provider。

**为何安全**：`Io` 不是 `Sync`（`NonNull` 为 `!Sync`），同一 `Io` 不会跨线程
共享。`MmioMapping` 在 `Io` 构造时产生、在 `Io::drop` 时消费，所有权单一。

### 2. `unsafe impl Send for DmaAllocation`（`src/lib.rs`）

```rust
unsafe impl Send for DmaAllocation {}
```

**不变量**：`DmaAllocation` 携带一致性缓冲区的地址值，所有权由 `DmaCoherent`
handle 独占，释放回到创建该 handle 的 provider。

**为何安全**：同上——`DmaCoherent` 非 `Sync`，handle 在线程间移动时，
缓冲区所有权随之转移。

### 3. `Io::access_ptr` 裸指针偏移（`src/lib.rs`）

```rust
unsafe { self.as_ptr().as_ptr().add(offset) }
```

**不变量**：
- `offset + size <= region.size`（由 `checked_add` + `assert!` 保证）。
- 基地址 `vaddr` 由提供者保证在映射期间有效。

**为何安全**：偏移量经过溢出检查和边界检查，结果指针不会越过映射区域。

### 4. `Io::read*` — `ptr.read_volatile()`（`src/lib.rs`）

```rust
let value = unsafe { ptr.read_volatile() };
```

**不变量**：
- 指针由 `access_ptr` 保证在映射区域内。
- 多字节读取时 `debug_assert` 检查自然对齐。

**为何安全**：`read_volatile` 不会导致 UB（指针有效且对齐），读回的值
由调用者解释。

### 5. `Io::write*` — `ptr.write_volatile(value)`（`src/lib.rs`）

```rust
unsafe { ptr.write_volatile(value) };
```

**不变量**：同 `read*`，指针在映射区域内且自然对齐。

**为何安全**：`write_volatile` 写入有效的 MMIO 映射地址。

## 内存安全不变量

以下不变量必须在任何时候都成立：

1. **映射有效性**：`Io` handle 持有的 `MmioMapping.vaddr` 在 handle
   构造到 drop 之间必须保持有效。由 `MmioOp` 实现保证。

2. **独占所有权**：同一映射区域不应被多个 `Io` handle 引用。
   由调用者保证不重复映射同一区域。

3. **边界检查**：所有 `read*`/`write*` 调用的 offset + size 不超过
   `region.size`。由 `access_ptr` 中的 `assert!` 运行时保证。

4. **对齐检查**：多字节 MMIO 访问必须自然对齐。
   由 `debug_assert!` 在 debug 构建中检查。

5. **DMA 缓冲区对齐**：`DmaAllocation.cpu_addr` 满足 `DmaSpec.align`
   要求。由 `DmaOp` 实现保证。

6. **提供者生命周期**：调用方传入的 provider 必须在所有由它创建的
   RAII handle 和 devres cleanup 回调结束前保持有效。当前 API 要求
   `&'static dyn ...` provider，以把该约束编码到类型中。

7. **MSI message 来源**：`MsiResource::message` 必须由 host IRQ core 或其
   backend provider 生成。driver 不应自行读取 APIC id、CPU vector 或其它
   controller-private 状态来构造 message。

## 线程安全

| 类型 | `Send` | `Sync` | 说明 |
|------|--------|--------|------|
| `MmioMapping` | 手动 `unsafe impl` | auto `!Sync`（`NonNull`） | 地址值可在线程间移动 |
| `DmaAllocation` | 手动 `unsafe impl` | auto `!Sync`（`NonNull`） | 地址值可在线程间移动 |
| `Io` | auto（字段均为 `Send`） | `!Sync`（`NonNull`） | 同一 handle 不可跨线程共享 |
| `Irq` | auto | auto | `IrqResource` 为 `Copy`，`bool` 为 `Send+Sync` |
| `DmaCoherent` | auto（字段均为 `Send`） | `!Sync`（`NonNull`） | 同一 handle 不可跨线程共享 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | MMIO 越界读写导致内核内存损坏 | 高 | 驱动传入的 `offset + size` 超出映射区域 | `access_ptr` 中 `checked_add` + `assert!` 运行时边界检查 |
| T-02 | 未对齐 MMIO 访问导致架构异常 | 高 | 驱动传入未对齐的 offset | `debug_assert!` 在 debug 构建中检查；release 构建依赖驱动正确性 |
| T-03 | 设备通过 DMA 修改内核内存 | 高 | 恶意或故障设备通过 DMA 缓冲区写入内核可见内存 | 一致性 DMA 缓冲区由驱动显式分配，设备可写范围限定在 `DmaSpec.len` 内；IOMMU 可进一步隔离 |
| T-04 | 中断风暴导致 CPU 资源耗尽 | 中 | 设备持续触发中断或中断未正确确认 | 由 `IrqOp` 实现负责限流和中断屏蔽；`Irq::set_enabled(false)` 可临时禁用 |
| T-05 | Drop 释放到错误 provider | 高 | acquire 使用一个 provider，drop 阶段重新查询到另一个 provider | RAII handle 保存创建时 provider，drop 回到同一 provider |
| T-06 | 重复映射同一 MMIO 区域 | 中 | 两个驱动请求重叠的 MMIO 区域 | 由 `MmioOp` 实现负责检测 `Busy` 并拒绝；本模块不做区域重叠检查 |
| T-07 | 固件提供恶意或错误的资源描述 | 高 | 恶意或 buggy 固件报告错误的 MMIO 基地址、零大小区域或无效 IRQ 号 | provider 实现应在 `map_mmio` / `request_irq` 中验证参数；本模块不校验 `ResourceDesc` 内容 |
| T-09 | driver 自行构造 MSI message | 高 | driver 使用当前 APIC id 或裸 vector 写 MSI-X table | SMP/affinity/x2APIC 下投递目标错误或 vector 冲突 | API 只提供 provider 生成的 `MsiResource::message`，不提供 APIC id 查询 |
| T-10 | MSI resource 隐式复制后重复释放 | 中 | resource 是 `Copy` 或被多个 owner 持有 | backend vector double-free 或 stale MSI message 被继续使用 | `MsiResource` 不实现 `Copy`，释放路径要求移动所有权 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `Io::map_with` 返回错误 | provider 拒绝映射或映射失败 | 驱动无法访问设备寄存器 | 设备不可用 | 3 | `ResResult` 强制错误处理 |
| F-02 | `Irq::request_with` 返回 `Busy` | provider 拒绝注册，例如共享 handler 达到上限或底层 IRQ 注册失败 | 驱动无法接收中断 | 设备中断功能不可用 | 3 | 返回 `ResError::Busy`，驱动终止 probe 或选择其他工作模式 |
| F-03 | `DmaCoherent::alloc_with` 返回 `NoMemory` | 内核内存不足 | DMA 缓冲区分配失败 | 依赖 DMA 的设备功能不可用 | 3 | 返回 `ResError::NoMemory`，驱动可降级运行 |
| F-04 | MMIO 读写时 offset 溢出 | `checked_add` 检测到 usize 溢出 | `assert!` panic | 调用线程 panic | 2 | `checked_add` 防止静默越界 |
| F-05 | MMIO 读写越界 | offset + size > region.size | `assert!` panic | 调用线程 panic | 2 | `assert!` 阻止实际越界访问 |
| F-06 | Drop 释放错误的共享 IRQ handler | 请求失败后仍构造 armed handle，或未保存提供者返回的 token | 当前 handler 未正确释放或同线其他 handler 被移除 | 设备中断功能异常 | 2 | 仅在 `request_irq` 成功后构造 `Irq`，并保存 token 供 Drop 精确释放 |
| F-07 | 在中断上下文中调用 `Io::map_with` / `Irq::request_with` / `DmaCoherent::alloc_with` | 驱动在中断处理程序中获取资源 | provider 方法可能执行睡眠或阻塞操作 | 系统挂起 | 1 | 文档约束 provider 方法仅可在正常上下文调用；无运行时检测 |
| F-09 | MSI resource 释放早于 IRQ handler 注销 | 驱动 teardown 顺序错误 | provider 可能释放 backend vector 时 OS-visible IRQ 仍被注册 | 后续 vector reuse 可能遇到旧 handler | 2 | host provider 应诊断该顺序问题；驱动应先 release handler 再 free MSI resource |

## 故障管理

`device-res` 通过以下机制处理故障：

- **错误传播**：所有资源获取函数返回 `ResResult<T>`，调用者通过 `?` 传播错误。
- **panic 路径**：MMIO 边界检查和溢出检查使用 `assert!`，失败时 panic。
  这些是编程错误，不可恢复。
- **Drop 配对**：所有 RAII handle 保存创建时 provider，drop 时回到同一 provider，
  避免 acquire/release 后端不一致。
- **MSI message 封装**：MSI-X 申请返回 provider 生成的 `MsiResource`，驱动只消费
  virq 和 message，不接触 irqchip-private vector/APIC id。
- **无错误码映射**：不返回 POSIX 错误码，使用 `ResError` 枚举。

## 隐私分析

本模块为硬件资源抽象层，不处理用户数据、不执行 I/O、不涉及网络通信。
不直接涉及用户隐私问题。DMA 缓冲区内容可能包含通过网络接收的数据，
但其生命周期管理由驱动负责。

## 已知限制

1. **区域重叠不检测**：本模块不检查多次映射的 MMIO 区域是否重叠。
   完全依赖 `MmioOp` 实现的 `Busy` 检查。

2. **release 构建无对齐检查**：MMIO 访问的对齐检查仅在 debug 构建中
   生效。release 构建中未对齐访问可能触发架构异常而不给出明确诊断。

3. **provider 需为 `'static`**：RAII handle 和 devres cleanup 闭包保存 provider
   引用，因此当前 API 不支持 probe 时临时构造的短生命周期 provider。

4. **`devm_*` 返回裸指针**：`devm_iomap` 返回 `NonNull<u8>`，
   调用者需自行保证不在设备移除后使用该指针。

5. **中断上下文约束未强制**：`IrqHandler::handle` 文档声明不可阻塞，
   但模块层无运行时检查。

6. **MSI-X 无 RAII handle**：当前 `MsiResource` 是显式 alloc/free 资源，不像
   `Irq` 一样自带 RAII Drop。它不实现 `Copy`，但调用方仍必须按 provider 合同释放。

## 审计清单

修改 `device-res` 时需验证：

- [ ] 每个 `unsafe impl Send/Sync` 均有 `SAFETY:` 注释说明不变量。
- [ ] `Io::access_ptr` 的边界检查覆盖所有 `read*`/`write*` 入口。
- [ ] 新增的 MMIO 访问方法包含 acquire/release fence。
- [ ] RAII handle 的 drop 路径使用创建该 handle 时保存的 provider。
- [ ] `Irq` 仅在 `request_irq` 成功后构造，并保存提供者返回的 handler token。
- [ ] 新增 MSI API 不暴露 APIC id、CPU vector 或 controller-private cookie。
- [ ] `devm_*` 函数在注册清理回调前完成资源获取（失败时不注册空回调）。
- [ ] 新增 provider trait 方法文档声明执行上下文约束。
