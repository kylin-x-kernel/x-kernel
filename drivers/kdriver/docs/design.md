# kdriver — 设计文档

## 定位

`kdriver` 是 x-kernel 的设备驱动编排 crate。
它拥有总线后端(bus backend)管理、驱动注册与匹配、设备发现→绑定→激活的完整流水线，
以及基于 devres 生命周期的资源管理(mmio / irq / dma)能力。
所有激活的运行时设备最终发布到对应类型的 `kclass` 注册表中。

目标读者是添加新总线后端、新设备驱动或修改设备发现/匹配策略的开发者。

## 背景

Linux 设备模型的核心是总线(bus)、设备(device)、驱动(driver)三者分离：
总线负责发现设备，驱动声明自己支持的设备特征，内核在中间完成匹配与绑定。
`kdriver` 把这一架构落实为 crate 级抽象：

- 总线后端抽象为 `BusBackend` trait，支持 PCI 和 platform 两种实例；
- 设备描述符(`DeviceDesc`)由总线发现阶段产生，不直接生成运行时对象；
- 驱动通过 `DeviceDriver` trait 声明匹配器和探测回调；
- `kdevice` 提供共享设备核心(device core)，管理 `BusInstance`、`DeviceObject`、`DriverObject` 的持久化拓扑；
- `kclass` 按设备类别(net / block / display / input / vsock / char / 9p)提供类型化发布入口。

## 范围

涉及的源文件：

```text
drivers/kdriver/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs                   # crate 入口，init_drivers、ownership summary API
    ├── manager.rs               # DeviceManager 与统一发现流水线
    ├── enumeration.rs           # EnumerationContext，总线后端写入的描述符缓冲
    ├── resource.rs              # x-kernel 资源提供者(MMIO/IRQ/DMA)，devm_* helpers
    ├── bus/
    │   ├── mod.rs               # 总线模块 root
    │   ├── backend.rs           # BusBackend trait
    │   ├── manager.rs           # BusManager 多后端管理
    │   ├── pci_backend.rs       # PCI 总线发现实现
    │   ├── pci_support.rs       # PCI BAR 分配与设备配置
    │   ├── platform_backend.rs  # Platform 总线(firmware + 静态设备)
    │   └── local_id.rs          # 总线后端本地 ID 分配器
    └── driver_registry/
        ├── mod.rs               # DriverRegistrar，ownership summary 类型
        ├── firmware_specs.rs    # 平台驱动的固件匹配规格
        ├── virtio/
        │   ├── mod.rs           # VirtIO 驱动描述符与激活逻辑
        │   ├── ids.rs           # VirtIO 设备类型码与 PCI ID 映射
        │   └── glue.rs          # VirtIoHal 实现，绑定 kdma/iomap
        ├── block/mod.rs         # 块设备驱动注册
        ├── char/mod.rs          # 字符设备驱动注册(console)
        └── net/mod.rs           # 网络设备驱动注册
```

## 架构

```text
                        init_drivers()
                             │
                    discover_unified()
                             │
                    ┌────────┴────────┐
                    │   DeviceManager │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
         BusManager    EnumerationContext  DriverRegistrar
              │              │              │
    ┌─────────┴─────────┐    │     ┌────────┴────────┐
    │                   │    │     │                 │
 PlatformBackend    PciBackend  │  virtio drivers   platform drivers
    │                   │       │  (net/blk/gpu/    (ramdisk/AHCI/
    │                   │       │   input/vsock/9p)  sdmmc/fxmac/…)
    │                   │       │
    └───────┬───────────┘       │
            │ probe             │
            ▼                   │
     ┌──────────────┐           │
     │  kdevice core │◄─────────┘
     └──────┬───────┘
            │ publish
            ▼
     ┌──────────────┐
     │ kclass 注册表 │
     │ net / block  │
     │ display      │
     │ input / vsock│
     │ char / 9p    │
     └──────────────┘
```

| 组件 | 职责 |
|------|------|
| `DeviceManager` | 持有 `BusManager` + `DriverRegistrar`，编排「发现→匹配→绑定→激活」完整流水线 |
| `BusManager` | 管理多个共存的后端实例；负责 `early_init` → `enumerate` → `rescan` → `quiesce` → `remove` 生命周期 |
| `BusBackend` | 总线后端 trait；实现者通过 `EnumerationContext` 写入 `DeviceDesc` |
| `EnumerationContext` | 描述符缓冲：记录发现阶段产生的设备描述符，随后执行统一 probe |
| `DriverRegistrar` | 驱动注册门面：把 `DeviceDriver` 实现注册到 `kdevice` 驱动核心 |
| `PlatformBackend` | 统一平台总线：firmware 描述的(device-tree / ACPI) + 编译期已知的静态设备 |
| `PciBackend` | PCI 总线：ECAM/MmioCam 枚举，BAR 分配，host bridge / PCI-to-PCI bridge adoption |
| `HostResourceProvider` | x-kernel 资源提供者实现；对接 `memspace`(iomap)、`kirq`(irq)、`kdma`(dma) |
| VirtIO 驱动族 | 每条 VirtIO 设备类型生成 PCI/MMIO 两个 `DeviceDriver` 描述符，共享同一激活路径 |
| Platform 驱动族 | ramdisk、AHCI、bcm2835-sdhci、sdmmc、fxmac 等平台设备驱动 |

## 初始化路径

初始化分为三条刻意分离的路径：

### 1. 平台早期初始化 (`early_driver_init`)

在通用驱动模型之前运行，负责 timer、IRQ 和 boot console 等必须提前工作的子系统。
此阶段不经过总线枚举——由平台 HAL 直接启动。

### 2. 描述符优先路径 (descriptor-first)

这是主路径，由 `init_drivers()` → `discover_unified()` 驱动：

1. **总线枚举**：`PlatformBackend` 和 `PciBackend` 各自枚举设备，通过 `EnumerationContext::register_device` 写入 `DeviceDesc` 到 `kdevice` 共享核心。
2. **驱动匹配**：`EnumerationContext::probe_pending()` 对每个描述符执行 `kdevice::probe_device_desc`，在已注册的 `DeviceDriver` 中按 `bus_type` + `matcher` 匹配。
3. **绑定与激活**：匹配成功后调用 `DeviceDriver::probe_device`，驱动在其中完成硬件初始化并把运行时设备发布到 `kclass` 注册表。
4. **未匹配设备**：没有匹配驱动的描述符进入 `unclaimed` 列表，日志记录但不阻断系统启动。

### 3. adoption 路径

预留给已在早期初始化中运行但缺少运行时设备模型对象的设备（如 boot console）。
通过 `kdevice::adopt_active_device` 把已运行的设备注册到设备树中，
使后续的 sysfs/设备查询路径能发现它。

## 状态机

### 设备核心状态

`kdevice` 中每个 `DeviceRecord` 经历以下状态迁移：

```text
                    ┌──────────┐
                    │Discovered│  ← 总线枚举 create
                    └────┬─────┘
                         │ driver matched
                         ▼
                    ┌──────────┐
                    │ Matched  │
                    └────┬─────┘
                         │ bind
                         ▼
                    ┌──────────┐
                    │  Bound   │
                    └────┬─────┘
                         │ activate (probe success)
                         ▼
                    ┌──────────┐
                    │  Active  │  ← 运行时设备已发布到 kclass
                    └────┬─────┘
                         │ remove
                         ▼
                    ┌──────────┐
                    │ Removing │
                    └────┬─────┘
                         │ cleanup done
                         ▼
                    ┌──────────┐
                    │ Removed  │
                    └──────────┘
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| — | Discovered | 总线后端 `enumerate` 调用 `register_device` |
| Discovered | Matched | `probe_device_desc` 找到匹配驱动 |
| Matched | Bound | 驱动绑定成功，设备-驱动关系建立 |
| Bound | Active | `DeviceDriver::probe_device` 返回 `Ok` |
| Matched/Bound/Active | Removing | `remove_device_managed` 被调用 |
| Removing | Removed | devres 清理完成，驱动 remove 回调执行完毕 |

未匹配的设备停留在 `Discovered` 但进入 `unclaimed` 列表，
不生成 `DeviceObject`。

### 总线后端生命周期

```text
BusManager::register(backend)
  │
  ▼
Registered ──► early_init() ──► enumerate() ──► probe_pending()
                   │                 │
                   │                 ├─ Activated (→ kclass publish)
                   │                 └─ Unclaimed (log + skip)
                   │
              rescan() ◄── 热插拔 / 重扫描
                   │
              quiesce() ──► 暂停事件 (shutdown / suspend)
                   │
              remove() ──► 总线拆除 (orderly shutdown)
```

## 算法流程

### PCI 总线枚举

1. 根据 `pci_cam_kind()` 选择 ECAM 或 MmioCam 访问方式。
2. 打开 `PciBus`，获取 config space 访问能力。
3. Adoption host bridge：在 platform 总线上创建 `pci-host` 设备作为 PCI 设备树的根。
4. **Pass 1**：遍历 bus 0..bus_end 所有 BDF，按 `HeaderType` 分流：
   - `Standard` → endpoint 列表
   - `PciPciBridge` → bridge 列表，读取 secondary/subordinate bus 号
   - 其他 → 跳过并记录日志
5. **Pass 2**：为每个 PCI-to-PCI bridge 调用 `adopt_active_device` 创建 `pci-bridge` 设备对象，
   同时创建对应的 secondary `BusInstance`。
6. **Pass 3**：遍历 endpoint 列表，对每个设备：
   - 调用 `configure_pci_device_if_needed` 分配未分配的 BAR、启用 command 寄存器
   - 读取 BAR 信息构建 `ResourceSet` (MMIO / IO Port / Legacy INTx IRQ)
   - 检测是否为 VirtIO-over-PCI 设备，若是则附加 `TransportInfo::Virtio`
   - 通过 `EnumerationContext::register_device_with_parent` 注册，parent 指向 host bridge 或 parent bridge

### Platform 总线枚举

1. **Firmware 阶段**：遍历 firmware 描述的设备节点 (DT compatible / ACPI)。
   - 对每个节点，查询已注册的 `FirmwareMatchSpec`（如 AHCI、sdmmc、fxmac 等）完成 compatible 匹配。
   - VirtIO MMIO 特殊处理：检测 `virtio,mmio` compatible，映射 MMIO 探测 VirtIO 设备类型。
   - 构建 `ResourceSet`（MMIO + IRQ），通过 `EnumerationContext` 注册。
2. **UART/serial 节点**：DT 中的 UART 节点（含 stdout）在此阶段被枚举。stdout UART 由 serial 驱动经 `take_early_port` 复用早期 boot 实例（同一硬件、不重新映射），其余 UART 各自映射并发布为独立 char 设备。
3. **静态设备阶段**：注册编译期已知的平台设备（ramdisk 始终注册；AHCI/sdmmc/bcm2835-sdhci 仅在无 firmware 描述时注册）。

   > ramdisk 的存储后端由 `KFEAT_DRIVER_RAMDISK_STATIC` 控制：关闭时为 16 MiB 全零堆内存（仅用于驱动验证）；开启时由构建期嵌入的文件系统镜像零拷贝承载（路径由 Makefile 变量 `RAMDISK_IMG` 指定，格式由 `RAMDISK_IMG_FS` 控制，默认 ext4，由 `make ramdisk_img` 生成空镜像），从而可挂载为真实的可读写 root 文件系统。详见 `drivers/block/src/ramdisk_image.rs`。

### 驱动匹配与激活

1. `EnumerationContext::probe_pending` 取出所有 pending 描述符。
2. 对每个描述符调用 `kdevice::probe_device_desc`：
   - 按 `bus_type` 筛选驱动
   - 调用 `DeviceDriver::matcher().matches(identity)` 判断是否匹配
   - 匹配时执行 bind → activate
3. 激活成功的设备：运行时对象已发布到对应 `kclass` 注册表。
4. 未匹配(`Unclaimed`)或被请求重排队(`Requeue`)的描述符进入 `unclaimed` 列表。

### VirtIO 设备激活

VirtIO 驱动在 PCI 和 MMIO 两条传输路径上共享同一激活入口：

1. **PCI 路径**：从 `DeviceLocation::Pci` 中提取 BDF，重新打开 `PciBus` 执行 `probe_pci_device`，
   确认传输层上报的 `DeviceKind` 与驱动声明一致。
2. **MMIO 路径**：从 `DeviceLocation::Mmio` 中提取物理地址和大小，`iomap_mmio` 后执行 `probe_mmio_device`。
3. **分发**：`dispatch_virtio_try_new` 按 `DeviceKind` 分发到对应构造器：
   - `DeviceKind::Net` → `VirtIoNet::try_new` → `kclass::publish_net`
   - `DeviceKind::Block` → `VirtIoBlk::try_new` → `kclass::publish_block`
   - `DeviceKind::Display` → `VirtIoGpu::try_new` → `kclass::publish_display`
   - `DeviceKind::Input` → `VirtIoInput::try_new` → `kclass::publish_input`
   - `DeviceKind::Vsock` → `VirtIoSocket::try_new` → `kclass::publish_vsock`
   - `DeviceKind::Fs9p` → `VirtIo9p::try_new` → `kclass::publish_virtio_9p`

### 资源管理 (devres)

`resource.rs` 提供 device-managed 资源分配，绑定到 `DeviceObject` 的 devres 清理链表：

- **`devm_iomap`**：通过 `memspace::iomap_device` 映射 MMIO，probe 失败或设备 remove 时自动 `iounmap`。
- **`devm_request_irq`**：注册中断处理函数到 provider 的共享 IRQ line state；每条 IRQ 由一个捕获 line state 的 handler 接入 `kirq`。
  `resource.rs` 是 devres IRQ resource 与 kernel IRQ core 的适配层，负责把
  `device_res` 的 trigger/controller/event/handler 转换到 `kirq` 自有类型；`kirq`
  不反向依赖 devres。
  后续 IRQ core 能力扩展必须放在 `kirq`，驱动框架只通过该适配层向驱动暴露内核
  IRQ 能力。
- **`devm_alloc_coherent`**：通过 `kdma::allocate_dma_memory` 分配一致性 DMA 缓冲区，release 时调用 `kdma::deallocate_dma_memory`。

释放顺序与申请顺序相反（LIFO），避免资源依赖错乱。

### IRQ 分发机制

`kirq::register` 存储 `Arc<dyn kirq::IrqHandler>`。`HostResourceProvider`
为每条 IRQ 创建一个共享 line state，并向 `kirq` 注册一个捕获该 state 的代理
handler：

1. `request_irq` 为设备 handler 分配 token，并将其加入对应 line state；首个 handler
   注册代理 handler，后续共享 handler 复用同一个代理。
2. 中断到达时，代理 handler 从 line state 复制最多 4 个 devres handler 到固定长度
   栈上快照，依次执行并合并 `device_res::IrqEvent`；事件被认领后将 source bitmap
   交给 `irq-notify`，再把结果转换成 `kirq::IrqEvent` 返回给 IRQ core。
3. `release_irq` 按 token 删除当前 handler；列表为空后注销 `kirq` handler 并删除 line state。

注册和释放路径允许分配，IRQ dispatch 路径不分配堆内存。

## 并发模型

- `DeviceManager::bus_mgr` 使用 `SpinNoPreempt`：总线枚举、重扫描、quiesce、remove 操作在 process context 中执行，互斥访问。
- `EnumerationContext` 自身没有 interior locking：它是总线后端和 probe 之间的单线程桥梁，仅在 `bus_mgr` 锁内被填充。
- `IRQ_LINES` 使用 `SpinNoIrq` 串行化 line state 的创建和销毁，每条 line state 使用独立的 `SpinNoIrq` 保护 handler 列表。
- `PCI_BAR_ALLOCATOR` 使用 `SpinNoPreempt`：BAR 分配仅在 process context（枚举或 probe）中发生。
- `kdevice` 共享核心的内部锁由 `kdevice` crate 自行管理，`kdriver` 不直接持有其锁。

## 设计决策

### 描述符优先，而非直接创建设备对象

**选择**：总线枚举阶段只产生 `DeviceDesc` 描述符，不直接创建 `DeviceObject`。

**Trade-off**：这增加了一次中间表示和批量 probe 步骤，但换取以下好处：

- 未匹配设备的描述符不浪费 `DeviceObject` 的内存和 devres 开销；
- 所有描述符在 probe 前已收集完毕，便于日志汇总和诊断未匹配设备；
- 未来可实现描述符去重（如 ACPI 和 DT 同时描述同一设备），在 probe 前合并。

**拒绝的方案**：Linux 内核的 `device_register` 模式——枚举即注册。
该模式在发现阶段直接创建设备对象，简化了代码路径，
但引入了注册与匹配的时序耦合（驱动必须在设备注册前加载）。
对于 x-kernel 当前所有驱动均为 built-in 的场景，描述符优先更简洁。

### 总线后端 trait 化替代编译期分支

**选择**：PCI 和 platform 总线实现为 `BusBackend` trait 的不同实现，
在 `default_bus_manager()` 中注册。

**Trade-off**：引入动态分发（`Box<dyn BusBackend>`）的微小开销，
但获得以下好处：

- 同一内核镜像可同时支持 PCI 和 platform 设备，不再需要编译期二选一；
- 新增总线类型（如 USB、I2C）只需实现 `BusBackend` trait 并注册，
  无需修改 `BusManager` 核心逻辑；
- 总线后端的生命周期管理（init/enumerate/rescan/quiesce/remove）统一在
  `BusManager` 中，减少了每个后端重复的调度代码。

**拒绝的方案**：编译期 `cfg` 分支在 `BusManager` 内硬编码单一总线类型。
该方案无动态分发开销，但无法同时支持多种总线，
且每增加一种总线就需要修改 `BusManager` 内部逻辑。

### PCI BAR 在枚举阶段一次性配置

**选择**：PCI 后端在枚举阶段完成 BAR 分配和 command 寄存器配置。

**Trade-off**：枚举阶段耗时略增（需遍历 endpoint 列表两次——先收集再配置），
但激活路径简化为纯读取：

- `configure_pci_device_if_needed` 在激活阶段因 BAR 已非零而退化为 no-op；
- 避免了激活路径中重新查找 firmware MMIO 窗口信息（`pci_bar_allocation_range`）；
- 如果配置失败，设备在枚举阶段就被跳过，不会产生未配置的 `DeviceDesc`。

**拒绝的方案**：延迟配置——在驱动 probe 阶段才分配 BAR。
该方案将 BAR 配置推迟到真正需要时，减少无用配置，
但增加了 probe 路径的复杂性（probe 需要持有 PCI bus lock 和 BAR allocator lock），
且配置失败时需要在 probe 中途回滚已注册的 `DeviceObject`。

### VirtIO PCI/MMIO 双描述符

**选择**：每个 VirtIO 设备类型生成两个 `DeviceDriver` 描述符（PCI 和 MMIO），
分别注册到各自的 bus type。

**Trade-off**：驱动描述符数量翻倍（6 种设备类型 × 2 = 12 个描述符），
但获得以下好处：

- 匹配器(`VirtioTypeMatcher`)基于 VirtIO 类型码而非 PCI vendor/device ID 或 DT compatible，
  PCI 和 MMIO 发现的设备都能正确匹配到对应的功能驱动；
- PCI 和 MMIO 激活路径的差异封装在 `activate_virtio_pci` / `activate_virtio_mmio` 中，
  上层 `dispatch_virtio_try_new` 完全统一，不关心传输类型。

**拒绝的方案**：单一描述符 + 运行时判断传输类型。
该方案减少了描述符数量，但要求匹配器同时比较 bus type 和 VirtIO type，
且 `probe_device` 入口需要分支处理 PCI 和 MMIO 两种传输初始化逻辑。

### IRQ handler 通过共享 line state 注册

**选择**：`kirq` 保存捕获共享 line state 的代理 handler，provider 通过 token
管理同一 IRQ 上的设备 handler。

**Trade-off**：首次注册一条 IRQ 时创建 line state，设备 handler 注册会扩展列表，
并获得以下特性：

- 设备 handler 自带上下文，不需要按 slot 生成 trampoline，也没有全局 IRQ 数量上限；
- token 允许释放单个共享 handler，其他同线设备继续接收中断；
- 固定长度栈上快照保证 dispatch 路径不分配。

**拒绝的方案**：预分配静态 `IrqSlot` 数组并生成 trampoline。该方案需要维护
slot 身份映射和硬编码 IRQ 总数上限。

## Drop / 资源释放

- devres 资源在 `DeviceObject` 的 remove 路径中按 LIFO 顺序释放。
- `PciBackend` 不持有需在 drop 中释放的持久资源（`PciBus` 在 `enumerate` 返回时释放）。
- `PlatformBackend` 仅持有 `LocalIdAlloc`（栈上 u16），无需显式释放。
- 中断释放按 token 删除设备 handler，最后一个 handler 删除后注销 `kirq` handler。
- 共享 DMA buffer 在 last handle drop 后由 `kdma::deallocate_dma_memory` 回收。

## Feature 门控关系

```text
virtio ────────► virtio-blk ──► block  + virtio + virtio/block
                virtio-net ──► net    + virtio + virtio/net
                virtio-gpu ──► display + virtio + virtio/gpu
                virtio-input ► input  + virtio + virtio/input
                virtio-socket► vsock  + virtio + virtio/socket
                virtio-9p ───► virtio + virtio/virtio-9p + kclass/virtio-9p

console ───────► console-pl011 / console-ns16550-mmio / console-ns16550-ioport
ramdisk ───────► block + block/ramdisk
ahci ──────────► any_firmware_driver + block
bcm2835-sdhci ─► any_firmware_driver + block
sdmmc ─────────► any_firmware_driver + block
fxmac ─────────► any_firmware_driver + net
ixgbe ─────────► net (placeholder)
```

`any_firmware_driver` 是一个 umbrella feature：当任一 DT/ACPI 平台驱动启用时为 true，
`cfg(feature = "any_firmware_driver")` 用于替代冗长的 `any(ahci, bcm2835-sdhci, sdmmc, fxmac)` 条件。
