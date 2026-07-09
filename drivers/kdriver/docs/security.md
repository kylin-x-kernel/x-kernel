# kdriver — 安全与可靠性分析

## 信任模型

```text
firmware (DT / ACPI) / PCI config space
   │
   │ untrusted: physical addresses, IRQ numbers,
   │            compatible strings, PCI vendor:device IDs
   v
┌─────────────────────────────┐
│ kdriver                     │
│                             │
│ safe boundary               │
│  ├─ BusManager 枚举调度     │
│  ├─ DriverRegistrar 注册    │
│  ├─ EnumerationContext 缓冲 │
│  └─ Ownership summary API   │
│                             │
│ unsafe boundary             │
│  ├─ HostResourceProvider    │
│  │   ├─ alloc_coherent      │
│  │   └─ free_coherent       │
│  ├─ VirtIoHalImpl           │
│  │   ├─ dma_alloc/dealloc   │
│  │   ├─ mmio_phys_to_virt   │
│  │   └─ share/unshare       │
│  ├─ AhciDriver / SdMmcDriver│
│  │   probe (MMIO → vaddr)   │
│  ├─ IxgbeHalImpl            │
│  │   dma + mmio translation │
│  └─ virtio::probe_mmio_device│
│      (raw MMIO register read)│
└──────────────┬──────────────┘
               │
               │ validated mappings, IRQ trampolines,
               │ DMA buffers
               v
    khal / kdma / memspace / driver crates
```

- `kdriver` 信任 `khal::irq::register` 校验中断线号合法性。
- `kdriver` 信任 `memspace::iomap_device` 拒绝映射到非法物理地址范围。
- `kdriver` 信任 `kdma::allocate_dma_memory` / `kdma::deallocate_dma_memory` 返回配对的有效 `(cpu_addr, bus_addr)`。
- `kdriver` 信任 `virtio` crate 的 `probe_mmio_device` 在访问 MMIO 寄存器前已完成必要的 volatile read 安全检查。
- `kdriver` 信任各 driver crate（`block::ahci`、`block::sdmmc`、`net::ixgbe`）对其 `new(vaddr)` 入口参数的 safety precondition 定义正确。
- 外部调用者（`kruntime` 启动路径）信任 `init_drivers` 在 platform `early_driver_init` 之后调用。

## 外部边界 / 攻击面

`kdriver` 是内核中接触硬件描述数据的核心 crate，
攻击面主要来自 firmware 提供的不可信物理地址、中断号和设备身份信息，
以及 PCI 设备 BAR 寄存器中的运行时配置值。

经检查，本模块直接或间接接触以下边界：

- **firmware 输入**：DT compatible strings、ACPI HID/CID、MMIO 物理地址、IRQ 线号、
  firmware source 类型（DeviceTree / ACPI）；
- **PCI 配置空间**：vendor:device ID、class/subclass、BAR 地址与大小、
  header type、bridge secondary/subordinate bus number、legacy INTx routing；
- **VirtIO MMIO 寄存器**：MagicValue、Version、DeviceID、VendorID 等探测寄存器；
- **设备驱动 probe 路径**：驱动通过 `iomap_first_mmio` / `devm_iomap` 映射的 MMIO 区域，
  以及 `devm_alloc_coherent` 分配的 DMA 缓冲区；
- **中断注册**：firmware 或 PCI INTx routing 提供的中断线号，
  通过 IRQ slot table 桥接到 `khal::irq`；
- **编译期静态配置**：`kbuild_config::AHCI_PADDR`、`kbuild_config::SDMMC_PADDR` 等平台固定地址。

本模块不直接解引用用户空间指针（设备发现均在 kernel process context 执行），
但会解析 firmware table 和 PCI config space 提供的物理地址，
因此物理地址校验和 MMIO 映射安全是核心关注点。

威胁分析重点应覆盖：

- firmware 提供的非法物理地址是否能通过 MMIO 映射覆盖内核关键数据结构；
- PCI BAR 分配后的零地址是否能被绕过，导致 page zero 映射；
- IRQ slot 注册与中断到达之间的竞态是否导致 handler 被错误调用或遗漏；
- DMA buffer 的 alloc/free 配对是否可能因布局不一致导致内存破坏；
- VirtIO HAL 实现的 share/unshare 配对是否能抵御设备侧恶意 DMA；
- 设备 remove 路径的 devres 释放顺序是否可能产生 use-after-free。

## unsafe 代码清单

### 1. HostResourceProvider — DMA 分配

位置：`src/resource.rs:144`

```rust
let info = unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
```

不变量：

- `layout` 由 `Layout::from_size_align(spec.len, spec.align)` 构造，`Layout` 构造函数已拒绝非法对齐和非零大小。
- 返回的 `DmaAllocation` 由 devres 独占持有，`DeviceObject` remove 时调用 `free_coherent` 释放。
- `cpu_addr` 和 `bus_addr` 描述同一块物理内存。

安全依据：

- `alloc_coherent` 通过 `Layout` 验证 size/align 合法性后再调用 `allocate_dma_memory`。
- `DmaAllocation` 无公开析构路径，唯一释放入口是 devres 回调中的 `free_coherent`。
- `kdma::allocate_dma_memory` 的 safety contract 要求 layout 有效且返回的 buffer 由调用者独占。

调用者：

- `devm_alloc_coherent` → 各设备驱动的 probe 路径。

### 2. HostResourceProvider — DMA 释放

位置：`src/resource.rs:163`

```rust
unsafe { kdma::deallocate_dma_memory(info, layout) };
```

不变量：

- `info` 和 `layout` 与当初 `alloc_coherent` 调用时一致。
- 每个 `DmaAllocation` 只释放一次（devres LIFO + 独占所有权）。
- 释放前设备 DMA 已停止（由驱动 remove 回调保证）。

安全依据：

- `free_coherent` 使用与 `alloc_coherent` 相同的 `Layout::from_size_align` 重建 layout。
- `DmaAllocation` 无 Clone，devres 持有唯一所有权。

调用者：

- `HostResourceProvider::free_coherent`，由 `device_res::DmaAllocation` 的 Drop 或 devres release 触发。

### 3. VirtIoHalImpl — unsafe trait 实现

位置：`src/driver_registry/virtio/glue.rs:139`

```rust
unsafe impl VirtIoHal for VirtIoHalImpl { ... }
```

不变量（整体）：

- `dma_alloc` / `dma_dealloc` 的 `(paddr, vaddr, pages)` 三元组配对一致。
- `mmio_phys_to_virt` 仅在 `PAGE_SIZE_4K` 对齐的物理地址上调用。
- `share` 和 `unshare` 成对调用，方向匹配。
- `dma_alloc` 分配后用 `write_bytes(0)` 清零，设备不会读到内核残留数据。

子项：

#### 3a. `dma_alloc`

位置：`src/driver_registry/virtio/glue.rs:148,150`

- `Layout::from_size_align(pages * PAGE_SIZE_4K, PAGE_SIZE_4K)` 构造后调用 `allocate_dma_memory`。
- 分配后 `write_bytes(0)` 清零：`unsafe { core::ptr::write_bytes(dma_info.cpu_addr.as_ptr(), 0, size) }`。
- 失败时返回 `(0, NonNull::dangling())`，由 VirtIO 传输层检测并报错。

#### 3b. `dma_dealloc`

位置：`src/driver_registry/virtio/glue.rs:165,178`

- 使用与分配相同的 `Layout`（`pages * PAGE_SIZE_4K` 对齐到 `PAGE_SIZE_4K`）。
- 从 `paddr` + `vaddr` 重建 `DMAInfo` 后调用 `deallocate_dma_memory`。

#### 3c. `mmio_phys_to_virt`

位置：`src/driver_registry/virtio/glue.rs:183`

```rust
unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
    iomap_mmio(paddr as usize, size, "virtio-mmio-hal")
        .expect("failed to iomap virtio MMIO region")
}
```

- `iomap_mmio` 内部调用 `memspace::iomap_device`，走标准 MMIO 校验路径。
- 如果映射失败则 panic——该路径仅在 VirtIO 传输层已确认设备存在后调用，映射失败意味着平台配置错误。

#### 3d. `share`

位置：`src/driver_registry/virtio/glue.rs:190,195`

```rust
unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection, ...) -> PhysAddr {
    unsafe { kdma::map_dma_buffer(buffer, dma_direction(direction)) }
        .expect(...)
        .bus_addr.as_u64() as PhysAddr
}
```

- `buffer` 来自 VirtIO 传输层分配的合法缓冲区（由 `dma_alloc` 或上层提供）。
- `direction` 正确映射到 `kdma::DmaDirection`。

#### 3e. `unshare`

位置：`src/driver_registry/virtio/glue.rs:203,209`

```rust
unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection, ...) {
    unsafe { kdma::unmap_dma_buffer(DmaBusAddress::new(paddr), buffer, dma_direction(direction)) };
}
```

- `paddr` 与 `buffer` 来自同一次 `share` 的返回值。
- 调用配对由 `virtio` crate 的传输层保证。

调用者：

- `virtio` crate 传输层（`VirtIoNetDev`、`VirtIoBlkDev`、`VirtIoGpuDev`、`VirtIoInputDev`、`VirtIoSocketDev`、`VirtIo9pDev`）。

### 4. VirtIO MMIO 探测（platform 枚举阶段）

位置：`src/bus/platform_backend.rs:193`

```rust
(unsafe { virtio::probe_mmio_device(regs.as_ptr(), mmio.size) })
```

不变量：

- `regs` 来自 `iomap_mmio(mmio.base, mmio.size, "virtio-mmio-discovery")`，已校验映射合法性。
- `mmio.size` 与映射时相同。
- 探测仅读取 VirtIO spec 定义的 MagicValue / Version / DeviceID 等只读寄存器。

安全依据：

- `iomap_mmio` 成功意味着物理地址在合法 MMIO 窗口内且映射已建立。
- `virtio::probe_mmio_device` 的 safety precondition 要求 `regs` 指向有效、可访问的 MMIO 区域且大小至少为一个 VirtIO MMIO register frame。

调用者：

- `PlatformBackend::enumerate_firmware` → `virtio_mmio_registration`。

### 5. VirtIO MMIO 探测（驱动激活阶段）

位置：`src/driver_registry/virtio/mod.rs:194`

```rust
unsafe { virtio::probe_mmio_device(regs.as_ptr(), size) }.ok_or(DriverError::BadState)?;
```

不变量：

- `regs` 来自 `iomap_mmio(base, size, "virtio-mmio-transport")`。
- 仅在 `DeviceLocation::Mmio` 且 transport 类型已匹配时进入此路径。
- `size` 来自 `DeviceLocation::Mmio.size`，与 platform 枚举阶段记录的一致。

安全依据：

- 同 #4。

调用者：

- `activate_virtio_mmio` → `activate_virtio_device` → VirtIO 驱动的 `probe_device`。

### 6. AHCI 驱动 probe

位置：`src/driver_registry/block/ahci.rs:25,63`

```rust
// line 25: LoongArch64 data cache barrier
unsafe { core::arch::asm!("dbar 0"); }

// line 63: construct AHCI driver from raw MMIO vaddr
let ahci = match unsafe { block::ahci::AhciDriver::<AhciHalImpl>::new(vaddr) } { ... };
```

不变量：

- `vaddr` 来自 `iomap_first_mmio(device, "ahci")`，通过 devres 管理生命周期。
- `iomap_first_mmio` 返回的指针在 `device` 存活期间有效。
- `dbar 0` 仅在 `target_arch = "loongarch64"` 时执行，用于 AHCI DMA coherency。

安全依据：

- `AhciDriver::new` 的 safety precondition 要求 `vaddr` 指向有效、独占的 AHCI HBA MMIO 窗口。
- `iomap_first_mmio` 通过 `devm_iomap` → `memspace::iomap_device` 确保映射有效性。

调用者：

- `AhciDriver::probe_device`（feature `ahci`）。

### 7. SDMMC 驱动 probe

位置：`src/driver_registry/block/sdmmc.rs:42`

```rust
let dev = unsafe { block::sdmmc::SdMmcDriver::new(vaddr) };
```

不变量：

- `vaddr` 来自 `iomap_first_mmio(device, "sdmmc")`。
- `SdMmcDriver::new` 的 safety precondition 要求 `vaddr` 指向有效的 SD/MMC 控制器寄存器区域。

安全依据：

- 同 AHCI 模式：`iomap_first_mmio` 保证映射有效性，devres 保证生命周期。

调用者：

- `SdmmcDriver::probe_device`（feature `sdmmc`）。

### 8. IxgbeHal — unsafe trait 实现

位置：`src/driver_registry/net/ixgbe_hal.rs:14`

```rust
unsafe impl IxgbeHal for IxgbeHalImpl { ... }
```

子项：

- `dma_alloc`（line 17）：`Layout::from_size_align(size, 8)` → `allocate_dma_memory`。
- `dma_dealloc`（line 23, 29）：重建 `Layout` → `deallocate_dma_memory`。
- `mmio_p2v`（line 33）：通过 `khal::mem::p2v` 做物理地址到虚拟地址的直接转换，假设调用者传入合法物理地址。
- `mmio_v2p`（line 37）：通过 `khal::mem::v2p` 反向转换。

不变量：

- DMA alloc/dealloc 的 `(paddr, vaddr, size)` 三元组配对。
- `mmio_p2v` / `mmio_v2p` 仅在 probe 阶段已确认物理地址有效后调用。

安全依据：

- `ixgbe` feature 当前为 placeholder（不启用下游依赖），HAL 实现不会被实际调用。

调用者：

- `ixgbe` crate 驱动（feature `ixgbe`，当前为 placeholder）。

## 内存安全不变量

1. **MMIO vaddr 生命周期**：`devm_iomap` 返回的 `NonNull<u8>` 仅在 `DeviceObject` 存活期间有效，probe 失败或设备 remove 时 `iounmap` 释放。
2. **DMA buffer 独占所有权**：`devm_alloc_coherent` 返回的 `DmaAllocation` 由 devres 独占持有，无公开 clone/复制接口。
3. **DMA alloc/free 配对**：`alloc_coherent` 和 `free_coherent` 使用相同的 `DmaSpec` 重建 `Layout`，保证 size/align 一致。
4. **IRQ slot 注册顺序**：先存储 handler 到 slot，再 `khal::irq::register` trampoline；`register` 失败时回滚清空 slot。
5. **IRQ slot 释放顺序**：先 `khal::irq::unregister` trampoline，再清空 slot，保证中断到达时要么找到有效 handler，要么 slot 为空。
6. **VirtIO DMA 清零**：`dma_alloc` 分配后用 `write_bytes(0)` 清零，防止设备读到内核残留数据。
7. **VirtIO share/unshare 配对**：`share` 和 `unshare` 成对调用，方向一致，由 `virtio` crate 传输层保证。
8. **PCI BAR 零地址拒绝**：枚举阶段分配后仍为 0 的 BAR 被跳过，不注册为有效资源。
9. **firmware 物理地址校验**：所有 firmware 提供的 MMIO 地址经 `memspace::iomap_device` 校验窗口合法性后再映射。
10. **设备身份白名单**：VirtIO PCI device ID 经 `pci_device_id_to_virtio_type` 白名单转换，未知 ID 的设备注册为通用 PCI 设备而不绑定 VirtIO 驱动。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `DeviceManager` | 字段满足 Send | `SpinNoPreempt<BusManager>` 提供内部可变性 |
| `BusManager` | `Vec<(BusId, Box<dyn BusBackend>)>` 满足 Send | 通过 `SpinNoPreempt` 提供共享访问 |
| `EnumerationContext` | `Vec<DeviceDesc>` 满足 Send | 不实现 Sync（单线程使用） |
| `DriverRegistrar` | 零大小类型 | 仅通过 `kdevice` 全局锁访问共享状态 |
| `HostResourceProvider` | 零大小类型 | 内部 IRQ slot 用 `SpinNoIrq` 保护 |
| `IrqSlot` | `SpinNoIrq<Option<...>>` 满足 Send + Sync | `SpinNoIrq` 提供内部可变性 |
| `PCI_BAR_ALLOCATOR` | `SpinNoPreempt<Option<PciRangeAllocator>>` 满足 Send + Sync | `SpinNoPreempt` 提供内部可变性 |
| `PlatformBackend` | 字段 `LocalIdAlloc` 为 `Copy`，满足 Send | 不实现 Sync（通过 BusManager 锁串行访问） |
| `PciBackend` | `Cam` 满足 Send | 不实现 Sync |
| `VirtIoHalImpl` | 零大小类型 | 内部调用 `kdma` / `iomap_mmio`，各自保证线程安全 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | firmware 提供非法物理地址，MMIO 映射覆盖内核关键数据结构 | 高 | DT/ACPI 描述恶意物理地址且 `memspace::iomap_device` 未拒绝 | `iomap_device` 校验地址是否在平台 MMIO 窗口内；非法范围返回 `InvalidRange` |
| T-02 | PCI BAR 分配后仍为零地址导致 page zero 映射 | 高 | BAR 分配器耗尽或分配范围未初始化，且 BAR 配置逻辑未拒绝零地址 | `configure_pci_device_if_needed` 分配失败返回 `NoMemory`；枚举 pass 3 跳过 `address == 0` 的 BAR |
| T-03 | IRQ slot 注册与中断到达竞态，handler 在未就绪时被调用 | 高 | 中断在 `register` trampoline 绑定后、handler 写入 slot 前到达 | handler 在 trampoline 绑定前写入 slot；`register` 失败时回滚清空 slot |
| T-04 | DMA double-free 导致内存破坏 | 高 | `free_coherent` 被多次调用或 layout 不匹配 | `DmaAllocation` 无 Clone，devres 独占所有权；`alloc`/`free` 使用相同 `DmaSpec` 重建 `Layout` |
| T-05 | VirtIO 设备通过恶意 DMA 描述符访问非授权内核内存 | 高 | 恶意或故障 VirtIO 设备构造错误描述符链 | 当前无 IOMMU 隔离单个 VirtIO 设备；`dma_alloc` 清零防止信息泄露；`share`/`unshare` 通过 `kdma` 管理 |
| T-06 | firmware 伪造设备 compatible 导致错误驱动绑定 | 中 | DT 提供虚假 compatible string 且恰好命中已注册的 `FirmwareMatchSpec` | 驱动 probe 会因硬件无响应而失败，设备进入 unclaimed 列表 |
| T-07 | PCI 设备伪造 vendor:device ID 触发错误 VirtIO 类型匹配 | 中 | 恶意 PCI 设备声明 Red Hat vendor ID 和已知 VirtIO device ID | `probe_pci_device` 在激活阶段二次验证传输层响应；不匹配时返回 `Unsupported` |
| T-08 | IRQ slot 耗尽导致合法设备无法注册中断 | 中 | 系统中激活超过 64 个中断驱动设备 | `request_irq` 返回 `ResError::NoMemory`，设备激活失败而非静默降级 |
| T-09 | PCI BAR 分配器竞态导致两个设备分配到相同 MMIO 地址 | 中 | 并发 BAR 分配未正确串行化 | `PCI_BAR_ALLOCATOR` 使用 `SpinNoPreempt` 保护，分配在锁内完成 |
| T-10 | stdout UART 的 MMIO 被 serial 驱动重复映射导致双重所有权 | 中 | serial 驱动对 stdout 节点再次调用 `devm_iomap` | serial 驱动 probe 经 `take_early_port` 按 `SerialIdent` 复用早期 stdout 实例，永不二次映射 |
| T-11 | devres 释放顺序错误导致设备仍在访问资源时资源被释放 | 中 | 驱动 remove 回调未停止设备 DMA 就返回 | devres LIFO 保证释放顺序；设备停止由驱动 remove 回调负责 |
| T-12 | VirtIO MMIO 探测读取未映射或无效寄存器 | 中 | firmware 描述 `virtio,mmio` compatible 但物理地址无 VirtIO 设备 | `probe_mmio_device` 先读 MagicValue 验证 VirtIO 协议；无效时返回 None |
| T-13 | firmware 描述的中断线号在平台 HAL 未校验时注册到错误向量 | 中 | `khal::irq::register` 未充分校验中断号 | 取决于平台 HAL 实现；`kdriver` 传入的 IRQ 号来自 firmware 或 PCI INTx routing |
| T-14 | 静态平台设备地址（AHCI_PADDR 等）编译期配置错误 | 低 | `kbuild_config` 常量配置了非法物理地址 | `iomap_first_mmio` 通过 `devm_iomap` → `iomap_device` 校验；映射失败导致驱动激活失败 |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | PCI 总线枚举失败 | ECAM/MmioCam 映射失败或 config space 不可访问 | PCI 设备全部不可用 | 依赖 PCI 设备的功能（virtio-blk/net/gpu/…）缺失 | 2 | `PciBus::new` 失败时记录 error 并返回，不阻断 platform 总线枚举 |
| F-02 | firmware 无设备描述 | DT/ACPI 表缺失或 `has_device_description()` 返回 false | firmware 枚举路径无设备注册 | 仅静态设备（ramdisk + 编译期配置的 AHCI/sdmmc）可用 | 3 | 静态设备路径独立于 firmware；记录 info 日志 |
| F-03 | 单个设备 probe 失败 | 驱动 `probe_device` 返回错误 | 该设备不可用 | 同总线其他设备正常激活 | 4 | probe 错误记录 warn 并进入 unclaimed 列表，不阻断后续设备 |
| F-04 | PCI BAR 分配器未初始化 | `pci_bar_allocation_range()` 返回 None 且设备有未分配 MEM BAR | 该 PCI 设备被跳过 | 单个 PCI 设备不可用 | 3 | `configure_pci_device_if_needed` 返回 `NoMemory`，设备跳过 |
| F-05 | VirtIO MMIO 探测返回空设备 | MMIO 区域不存在 VirtIO 设备或 MagicValue 不匹配 | 该 MMIO 区域跳过 | 不影响其他 platform 设备 | 4 | `probe_mmio_device` 返回 None → `virtio_mmio_registration` 返回 None，记录 trace 后跳过 |
| F-06 | IRQ trampoline 未注册 | slot 分配失败（64 槽位满） | 设备无法接收中断 | 该设备功能不可用或降级到轮询 | 3 | `request_irq` 返回 `NoMemory` 或 `Busy`，驱动 probe 返回错误 |
| F-07 | PCI host bridge adoption 失败 | platform 总线未注册或 `adopt_active_device` 错误 | PCI 设备无 host bridge parent | PCI 端点仍被枚举但设备树不完整 | 3 | adoption 失败记录 warn，枚举继续（parentless 布局） |
| F-08 | 静态设备 MMIO 映射失败 | `kbuild_config` 地址非法或硬件不存在 | 该静态设备不可用 | 同总线其他设备正常 | 3 | `iomap_first_mmio` 返回错误，probe 失败 |
| F-09 | 驱动注册时 bus type matcher 未就绪 | `register_bus_type` 在 driver 注册后调用 | 驱动匹配不到设备 | 设备进入 unclaimed 列表 | 2 | `default_bus_manager` 先注册 bus type matcher，再注册 bus backend，再在 `DeviceManager::new` 中注册 driver |
| F-10 | rescan 产生重复设备描述符 | 后端未覆盖 `rescan` 钩子，重走完整 `enumerate` | `kdevice` 可能拒绝重复注册或产生冗余描述符 | 热插拔功能不完整 | 3 | 默认 `rescan` 重走 `enumerate`；后端可按需覆盖实现增量扫描 |
| F-11 | VirtIO 传输层类型与驱动声明不匹配 | 设备上报的 `DeviceKind` 与匹配驱动的 `device_type` 不一致 | 驱动 probe 返回 `Unsupported` | 该设备进入 unclaimed | 4 | `activate_virtio_device` 在 PCI/MMIO 激活路径中做二次类型校验 |
| F-12 | quiesce 未停止设备中断 | 总线后端 `quiesce` 未正确实现或硬件响应延迟 | 中断在 shutdown 期间继续到达 | IRQ handler 可能访问已释放的资源 | 2 | devres 在 remove 而非 quiesce 阶段释放；quiesce 仅屏蔽中断源 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- 设备 probe 失败使用 `DriverError` 返回（`InvalidInput`、`Io`、`NoMemory`、`ResourceBusy`、`Unsupported`、`BadState`），不 panic。
- PCI 总线初始化失败记录 `error!` 并返回 `Ok(())`，不阻断 platform 总线继续枚举。
- firmware 枚举中的单个设备注册错误被收集（`first_error.get_or_insert`），枚举完成后返回第一个错误。
- MMIO 映射失败通过 `memspace::IoMapError` → `ResError` / `DriverError` 逐层转换，各层均可追踪。
- IRQ 注册失败返回 `ResError::Busy`（线被占用）或 `ResError::NoMemory`（slot 满），上层 probe 据此返回错误。
- 未匹配设备进入 `unclaimed` 列表并通过 `info!` 记录 identity + location + origin，便于诊断缺失驱动。
- 除 `VirtIoHalImpl::mmio_phys_to_virt` 中的 `expect`（仅在 VirtIO 传输层已确认设备存在后调用）外，所有 unsafe 块的错误路径返回 `Result` 或记录错误日志。
- panic 路径主要来自 `LazyInit::call_once` 后的 `expect`（静态初始化失败意味着平台配置错误）和 `LocalIdAlloc::alloc` 溢出（u16 溢出意味着设备数量异常）。

## 隐私分析

`kdriver` 处理 firmware table 中的设备身份信息（compatible string、ACPI HID/CID、PCI vendor:device ID 及其 class/subclass）和物理地址 / 中断号等硬件资源描述数据。
这些数据在日志中以 debug/info 级别输出设备名称、BDF 地址、物理地址范围和中断线号，
不包含用户进程数据。

模块自身不做持久化存储；设备拓扑信息保存在 `kdevice` 共享核心中，
生命周期受全局设备注册表管理。

trace 日志会输出 firmware 遍历的 compatible string 和 VirtIO MMIO 探测的寄存器值，
生产环境需按日志级别控制。

## 已知限制

- PCI segment 仅支持 segment 0，多 segment 系统需要扩展 `PciBackend` 的 domain 参数。
- PCI BAR 分配器使用简单的顺序分配（`PciRangeAllocator`），不支持 BAR 重定位和碎片整理。
- IRQ slot 上限为硬编码 64，超出时设备激活失败，无动态扩容机制。
- `ixgbe` feature 为 placeholder，其 HAL 实现未被实际调用，安全性未在运行时验证。
- `fxmac` 在没有 firmware 描述时仅记录 warn 并跳过，无法从编译期配置获取 MMIO 基址。
- firmware 枚举不处理 ACPI _DSD 或复杂设备属性，仅读取 compatible/HID/CID、MMIO 和 IRQ 资源。
- PCI hot-plug 仅通过 `rescan` 全量重枚举支持，无原生的 hot-plug event 处理（无 PCIe AER / hot-plug controller 驱动）。

## 审计清单

修改本模块时需验证：

- 每个 `unsafe` 块均有 `SAFETY:` 注释。
- 新增 MMIO 映射路径使用 `devm_iomap` 或 `iomap_mmio`（内部校验物理地址合法性）。
- 新增 DMA 分配路径通过 `devm_alloc_coherent` 或 `kdma::allocate_dma_memory`，且 `free` 时 layout 一致。
- 新增 IRQ 注册遵循「先写 slot → 再 bind trampoline → 失败回滚」顺序。
- 新增 IRQ 释放遵循「先 unbind trampoline → 再清空 slot」顺序。
- 新增总线后端实现 `BusBackend` 时，`enumerate` 中对每个设备的错误不应阻断其他设备枚举。
- 新增 PCI device ID 到 VirtIO 类型的映射在 `pci_device_id_to_virtio_type` 中添加。
- 新增 firmware compatible 匹配规格在 `firmware_specs.rs` 中声明，并注册对应的 platform driver。
- 新增 `DeviceDriver` 的 `probe_device` 在失败时返回 `DriverError` 而非 panic。
- 新增 `VirtIoHal` 实现方法需保证 `share`/`unshare` 配对和 `dma_alloc`/`dma_dealloc` 配对。
