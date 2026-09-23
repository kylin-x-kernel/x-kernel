# vdev-virtio-blk — 设计文档

## 定位

`vdev-virtio-blk` 是 X-Kernel 的 VirtIO MMIO 块设备模型 crate。它把
`vdev-virtio` 提供的传输/队列基础设施组装成一个可用的块设备：设备配置空间
（容量、几何、I/O 限制）、请求数据路径（OUT/IN/FLUSH）以及可插拔的
`BlockBackend`。`bus::VirtioBlkDevice` 再把该模型适配为
`vdev_core::MmioDevice`，注册到 VMM 的 MMIO 总线。

本 crate 属于可复用虚拟设备层：只依赖 `vdev-core`、`vdev-virtio` 与同步原语，
不依赖 `kvmm`、体系结构 HAL 或内核内存管理。VM 只需提供 `GuestDma` 翻译能力与
`IrqSender` 注入能力。

## 背景

原实现位于 `kvmm` 的 `src/vdev/virtio_blk/` 与 `virtio_blk_bus.rs`。设备模型
本身只依赖 `vdev-core` 接口，`bus.rs` 也只用
`vdev_core::{GuestDma, IrqSender, MmioDevice}`，因此整体迁出后 `kvmm` 不再持有
VirtIO 设备代码，仅 `kvmm-api` 在装配 VM 时构造并注册该设备。

## 范围

```
virt/vdev/virtio-blk/
├── src/
│   ├── lib.rs          # crate 入口与 re-export
│   ├── backend.rs      # BlockBackend trait
│   ├── bus.rs          # VirtioBlkDevice + RamDiskBackend（MmioDevice 绑定）
│   ├── selftest.rs     # 宿主机驱动序列自测
│   ├── block/config.rs # VirtioBlockConfig 配置空间
│   ├── constants/      # 块设备常量（复导出 vdev-virtio 通用常量）
│   └── mmio/           # VirtioMmioBlockDevice + BlockDeviceEvent + 请求头
└── Cargo.toml
```

| 组件 | 职责 |
|------|------|
| `mmio::VirtioMmioBlockDevice<B, T>` | 组合 `VirtioMmioState<T>` 与块配置，处理配置空间读与请求队列 |
| `backend::BlockBackend` | 存储后端契约：`read`/`write`/`flush` 与是否延迟处理 |
| `bus::RamDiskBackend` | 同步内存 RAM 盘后端（默认 16 MiB） |
| `bus::VirtioBlkDevice` | 以 `GuestDma`/`IrqSender` 实现 `MmioDevice`，转发 MMIO 与 used-ring 中断 |
| `mmio::BlockDeviceEvent` | MMIO 写之后 VMM 需要执行的动作 |
| `selftest::vmm_selftest_virtio_blk` | 不开 VM 的完整驱动序列自测 |

## 架构

```
┌───────────────────────────────────────────┐
│            VMM MMIO bus（vdev-core）        │
│        MmioDevice::read/write_for_vcpu     │
└───────────────┬───────────────────────────┘
                │
┌───────────────┴───────────────────────────┐
│           bus::VirtioBlkDevice             │
│  地址换算 → mmio_read/mmio_write_with_memory│
│  事件翻译 → IrqSender / deferred latch      │
└───────────────┬───────────────────────────┘
                │ DmaGuestMemory(vdev-virtio)
┌───────────────┴───────────────────────────┐
│      mmio::VirtioMmioBlockDevice<B, T>     │
│  VirtioMmioState(标准寄存器) + 块配置空间    │
│  请求解析：header / data / status 描述符链   │
└───────────────┬───────────────────────────┘
                │ BlockBackend
┌───────────────┴───────────────────────────┐
│   RamDiskBackend / 其他后端实现             │
└───────────────────────────────────────────┘
```

## 调用约束 / 执行上下文

- **MMIO 读写来自 vCPU 退出路径**：任务上下文，非中断上下文；`write_for_vcpu`
  会以内联方式处理同步后端的队列。
- **同步后端内联处理**：`RamDiskBackend` 的 `requires_deferred_processing()`
  为 `false`，`QUEUE_NOTIFY` 在 MMIO 写路径内完成读写并立即注入 used-ring 中断。
  若后端改为可能阻塞，`VirtioMmioBlockDevice` 返回 `QueuePending`，需要 VMM 提供
  轮询路径；当前 `VirtioBlkDevice` 会用 `deferred_pending` 位防御性记录。
- **访客内存通过 `GuestDma` 注入**：`GuestPhysAddr` 先换算为设备内偏移，再交给
  `DmaGuestMemory` 读写；不依赖宿主直接映射假设。
- **中断注入由 `IrqSender` 完成**：目标 vCPU 与 IRQ 号由调用方在构造时决定
  （默认 slot 1：base `0x0a00_1000`、IRQ 49）。
- **不睡眠、不依赖调度器**：可在启动期构造并注册。

## 状态机

### 请求处理

```
Idle ──QUEUE_NOTIFY──> Process ──同步后端──> Complete ──used ring + IRQ──> Idle
                          │
                          │ 后端 WouldBlock
                          └──> QueuePending（交由轮询/延迟路径）
```

### 传输复位

```
Active ──status 写 0──> Reset（丢弃 pending_head）──> Idle
```

`pending_head` 记录后端延迟处理时被取出但未完成的描述符；复位必须清除它，避免
复位后处理失效请求。

### 后端延迟策略

- `requires_deferred_processing() == false`：内联处理，MMIO 写返回
  `BlockDeviceEvent::InterruptPending` 或 `None`。
- `== true`：MMIO 写返回 `QueuePending(index)`，由运行时调用
  `process_pending_queue`。

## 算法流程

### 配置空间读

1. `VirtioMmioState::mmio_read` 处理标准寄存器。
2. 配置区读交由 `read_config_space`：容量、`size_max`、`seg_max`、几何、
   block size 等。

### 请求处理（`handle_queue_notify`）

1. 校验 `DRIVER_OK` 与队列有效性。
2. 循环取 head（优先 `pending_head`，否则从 available ring 弹出）。
3. `process_request`：
   - 校验描述符链长度 ≥ 2；首个描述符为设备可读的请求头；末个为可写的 status。
   - 解析 `request_type` 与 `sector`。
   - `IN` → 后端读入 buffer 并写回各数据描述符；`OUT` → 从数据描述符读入 buffer
     并写后端；`FLUSH` → `backend.flush()`。
   - 回写 status（OK/IOERR/UNSUPP）；`WouldBlock` 返回 `None` 挂起。
4. `complete_with_memory` 写 used ring；必要时 `trigger_interrupt` 置中断位。

### 地址与容量校验

- `validate_data_request` 检查描述符数量/单段长度、总长度按 512 字节对齐、
  请求区间落在容量内；失败返回 `InvalidBufferSize`/`InvalidSector`，并以 IOERR
  完成请求而非 panic。

## 并发模型

- `VirtioMmioBlockDevice` 内部状态由 `VirtioMmioState` 的 `SpinNoIrq` 锁保护；
  `pending_head` 另由 `SpinNoIrq<Option<u16>>` 保护。
- `Arc<T>` accessor 共享；`RamDiskBackend` 用 `kspin::SpinNoIrq<Vec<u8>>` 串行化
  内存盘访问。后端 I/O 在队列 `SpinNoIrq` 锁内执行，因此后端锁也必须是不可睡眠
  的自旋锁：`ksync::Mutex` 在竞争时会阻塞当前任务，而此刻本地中断已被屏蔽，
  可能死锁。
- `VirtioBlkDevice` 的 `deferred_pending` 是 `AtomicBool`，仅作防御性记录。
- 中断注入在锁外调用 `IrqSender::inject`。

## 设计决策

- **设备模型与总线绑定都放在本 crate**：`bus::VirtioBlkDevice` 只依赖
  `vdev-core` trait，放在 `kvmm` 会无谓地把通用代码绑到 host crate。
- **默认 base/IRQ 固化在 `bus.rs`**：沿用 QEMU `virt` 平台 `virtio_mmio` slot 1
  （`0x0a00_1000`、IRQ 49），slot 0 留给 `vdev-virtio-net`。设备可被多 VM 复用，
  VM 侧通过各自 `GuestDma`/`IrqSender` 隔离。
- **同步 RAM 盘为默认后端**：便于启动期即完成数据路径，不需要延迟处理机制。
- **自测放在 crate 内**：`selftest` 以假 `GuestDma` + 记录型 `IrqSender` 驱动完整
  序列，可由内核 unittest 直接调用（`vdev_virtio_blk::selftest::vmm_selftest_virtio_blk`）。

## Drop / 资源释放

无自定义 `Drop`。`Arc<VirtioMmioBlockDevice>`、`RamDiskBackend` 的内存盘与队列
随结构体析构释放。设备从 MMIO 总线移除（VMM 释放 `Box<dyn MmioDevice>`）后不再
接收访问；在途请求依赖同步后端已内联完成，复位会丢弃 `pending_head`。