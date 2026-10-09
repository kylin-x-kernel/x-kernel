# vdev-virtio — 设计文档

## 定位

`vdev-virtio` 是 X-Kernel 的 VirtIO MMIO 设备模型基础设施 crate：提供标准 MMIO
传输寄存器状态机、split virtqueue（描述符表 / available ring / used ring）以及
设备配置空间的通用抽象。具体设备（如 `vdev-virtio-blk`）在其之上只实现
设备特定的配置空间与数据路径。

本 crate 属于可复用的虚拟设备层，由 `vdev-core` 提供的接口（`GuestDma`）与
`kspin` 提供的中断屏蔽锁支撑。它不依赖任何 host VMM crate（`kvmm`）、体系结构
HAL 或内核内存管理，因此可被不同 VMM 复用，也可在宿主机测试中单独驱动。

## 背景

原有实现位于 `kvmm` 的 `src/vdev/virtio/` 中。代码本身只引用自身模块与
`alloc`/`core`/`kspin`/`log`，既非体系结构相关也非 KVMM 相关；真正与 KVMM
耦合的只有“访客物理地址 → 主机可解引用地址”的翻译能力。把该设备模型提取为
独立 crate 后，`kvmm` 只保留 `GuestDma` 实现与体系结构相关设备，边界更清晰。

## 范围

```
virt/vdev/virtio/
├── src/
│   ├── lib.rs            # crate 入口与公共类型 re-export
│   ├── addr.rs           # GuestPhysAddr / PhysAddr / AccessWidth
│   ├── config.rs         # 设备配置空间视图
│   ├── device_type.rs    # VirtioDeviceId
│   ├── dma.rs            # vdev_core::GuestDma → GuestMemory 适配器
│   ├── error.rs          # VirtioError / VirtioResult
│   ├── memory.rs         # GuestMemory / GuestMemoryAccessor
│   ├── constants/        # MMIO 寄存器、队列、通用常量
│   ├── mmio/             # transport 寄存器解码 + VirtioMmioState 状态机
│   └── queue/            # 描述符链、available/used ring、VirtioQueue
└── Cargo.toml
```

## 架构

```
┌──────────────────────────────────────────────┐
│  vdev-virtio-blk / vdev-virtio-net（设备实现）  │
│  设备配置空间 + 请求数据路径                    │
└───────────────┬──────────────────────────────┘
                │ VirtioMmioState<T> / VirtioQueue<T>
┌───────────────┴──────────────────────────────┐
│              vdev-virtio（本 crate）           │
│  mmio::state   标准 MMIO 寄存器状态机          │
│  mmio::transport  寄存器解码与校验             │
│  queue        描述符表 / avail / used ring     │
│  memory       GuestMemory / GuestMemoryAccessor│
│  dma          DmaGuestMemory（GuestDma 适配）   │
└───────────────┬──────────────────────────────┘
                │ vdev_core::GuestDma
┌───────────────┴──────────────────────────────┐
│  VM 的访客内存翻译（提交方实现 GuestDma）        │
└──────────────────────────────────────────────┘
```

| 组件 | 职责 |
|------|------|
| `mmio::state::VirtioMmioState<T>` | 维护 status、feature 选择器、queue 选择器、中断状态、config generation 等标准寄存器，并持有队列向量 |
| `mmio::transport` | 校验访问宽度、判断寄存器是否落在设备配置区、解码寄存器偏移 |
| `queue::VirtioQueue<T>` | 队列配置（desc/avail/used 地址与 size）、ready 校验、描述符链遍历、used 完成回写 |
| `queue::{AvailableRing, UsedRing, DescriptorTable}` | 三类环结构的布局计算与读写 |
| `memory::GuestMemory` | 队列/数据路径所需的最小访客内存读写能力 |
| `memory::GuestMemoryAccessor` | 可做“地址翻译并返回连续上限”的访问者，供 `AddressSpaceMemory` 适配 |
| `dma::DmaGuestMemory` | 把 `vdev_core::GuestDma` 适配成 `GuestMemory`，是生产路径的默认实现 |
| `constants` | MMIO 寄存器偏移、队列常量、feature/status/descriptor 位 |

## 调用约束 / 执行上下文

- **非中断上下文**：MMIO 读写与队列处理由 VMM 在访客退出路径上调用，属于任务
  上下文，不要求中断屏蔽；内部锁使用 `kspin::SpinNoIrq`，临界区只覆盖寄存器与
  队列元数据，不包含访客内存大块拷贝之外的等待。
- **不可睡眠**：本 crate 不做任何阻塞等待；后端若可能阻塞，必须由设备实现把
  `QUEUE_NOTIFY` 转为延迟处理（见 `vdev-virtio-blk` 的 `QueuePending`）。
- **访客内存能力由调用方注入**：生产路径传入 `GuestDma` 支持的
  `DmaGuestMemory`；仅做布局计算、不处理队列的场景可传
  `NoGuestMemoryAccessor` 并使用 `_with_memory` 形式的分步操作。
- **无周期/调度依赖**：不访问 CPU-local 状态、不依赖调度器或平台初始化，可在
  早期启动或宿主机测试环境运行（只要 `alloc` 可用）。
- **可重入**：对同一 `VirtioQueue`/`VirtioMmioState` 的并发访问由内部锁串行化；
  不同设备实例之间相互独立。

## 状态机

### 设备 status 寄存器

```
0 ──ACKNOWLEDGE──> ACK ──DRIVER──> ACK|DRIVER ──FEATURES_OK──> ... ──DRIVER_OK──> Ready
│                                                                           │
└────────────────────────── status 写 0（reset）─────────────────────────────┘
```

- 驱动写 `FEATURES_OK` 时校验 `driver_features` 是 `device_features` 的子集；
  不满足则清掉 `FEATURES_OK`。
- 写 status `0` 触发 `MmioWriteAction::Reset`。
- `is_driver_ok()` 供设备实现判断是否可处理队列。

### 队列配置

```
Unset ──QUEUE_SEL/QUEUE_NUM/QUEUE_DESC|AVAIL|USED──> Programmed ──QUEUE_READY=1──> Ready
   ^                                                                                  │
   └────────────────────────── reset / QUEUE_READY=0 ─────────────────────────────────┘
```

`QUEUE_READY` 写入时根据 desc/avail/used 地址与 size 计算各环布局区间，任何
越界或地址重叠都拒绝置 ready。

### 中断状态

- `set_interrupt(bits)`：`pending |= bits` 且 `raised_after_read |= bits`。
- 驱动读 `INTERRUPT_STATUS` 后，`INTERRUPT_ACK` 清除 `pending` 对应位。
- 若在读之后又有位被置位，`mmio_write` 返回 `MmioWriteAction::InterruptPending`，
  设备实现据此再次注入中断，避免漏报。

## 算法流程

### MMIO 读

1. `transport` 校验访问宽度。
2. 命中标准寄存器 → 返回 `MmioReadOutcome::Standard`。
3. 命中设备配置区 → 返回 `MmioReadOutcome::DeviceConfig { offset, width }`，
   由设备实现解释其配置空间。

### MMIO 写

1. `transport` 校验宽度并解析寄存器。
2. 写 feature/queue 寄存器更新 `VirtioMmioState`；`QUEUE_READY` 做环布局校验。
3. `QUEUE_NOTIFY` → `MmioWriteAction::QueueNotified(index)`。
4. status 写 0 → `MmioWriteAction::Reset`。

### 队列数据路径

1. 设备实现从 available ring 弹出 head 描述符索引。
2. 遍历描述符链（`VIRQ_DESC_F_NEXT` / `VIRQ_DESC_F_INDIRECT`），构造
   `DescriptorChain`。
3. 按设备语义读写描述符缓冲。
4. 通过 `complete_with_memory` 把 head 与写入长度写入 used ring 并推进 used idx。

## 并发模型

- `VirtioMmioState` 中每个可变寄存器与队列向量各由一把 `SpinNoIrq` 锁保护；
  锁粒度小、仅在寄存器更新/队列元数据访问时持有。
- `VirtioQueue` 内的 accessor 以 `Arc<T>` 共享，`T: Clone`；队列自身通过
  `queues_lock()` 暴露给设备数据路径，保证同一时刻只有一个数据路径在遍历。
- 中断注入/后端回调不在本 crate 内，由设备实现决定；本 crate 不直接触碰硬件。

## 设计决策

- **保留 `GuestMemoryAccessor` 与 `GuestMemory` 两个层次**：前者用于需要“翻译 +
  连续上限”的宿主测试/独立设备模型，后者是队列操作真正需要的最小能力。
- **以 `DmaGuestMemory` 收敛到 `vdev-core`**：生产路径不再各设备自定义访客内存
  适配器，统一走 `GuestDma`，使本 crate 只依赖 `vdev-core` 接口。
- **不把 crate 放进 `vdev-core`**：`vdev-core` 只放跨 VMM 的 trait 契约；
  VirtIO 是“实现族”，作为 `vdev-core` 的兄弟 crate 更符合边界划分。
- **从 `kvmm` 迁出**：设备模型与 VMM 无关，迁出后 `kvmm` 只剩 `GuestDma`
  实现、world-switch hook 与体系结构设备。

## Drop / 资源释放

本 crate 无自定义 `Drop`。队列与访问者随所属设备结构体析构；`Arc<T>` 访问者在
最后一个引用释放后回收。访客内存由 `GuestDma` 的拥有者（VMM）管理，本 crate
只做作用域内的读写，不持有宿主内存所有权。