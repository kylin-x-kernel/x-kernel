# VirtIO — 安全与可靠性分析

## 信任模型

```
上层子系统（块设备层 / 网络栈 / 显示框架 / 输入子系统 / vsock / 9p）
   │
   │ safe API: VirtIoBlkDev, VirtIoGpuDev, VirtIoInputDev,
   │          VirtIoNetDev, VirtIoSocketDev, VirtIo9pDev
   │          probe_pci_device()
   │ unsafe API: probe_mmio_device()
   │
   v
┌──────────────────────────────────────────────────────────────┐
│  virtio                                                      │
│                                                              │
│  ┌── unsafe 边界 ──────────────────────────────────────────┐ │
│  │ probe_mmio_device: MmioTransport::new()                 │ │
│  │ net.rs: NetBuf::from_handle()                           │ │
│  │ net.rs: receive_begin() / receive_complete()            │ │
│  │ net.rs: transmit_begin() / transmit_complete()          │ │
│  │ 各模块: unsafe impl Send/Sync                           │ │
│  └─────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

- **safe API 调用者**：信任模块正确维护不变量，设备操作不会导致 UB。
- **unsafe API 调用者**：`probe_mmio_device` 的调用者需确保传入有效的 MMIO 地址；
  `probe_pci_device` 的调用者需确保 PCI 配置空间可访问。

## 外部边界 / 攻击面

`virtio` 是典型的硬件/虚拟硬件边界模块。
它直接连接 VMM 暴露的 VirtIO 设备、
平台传输层和上层驱动框架，
因此攻击面不能只看 Rust `unsafe`。

经检查，本模块直接接触以下边界：

- **MMIO / PCI 配置空间**：
  设备 header、BAR、capability、IRQ 路由；
- **DMA / 共享内存缓冲区**：
  virtqueue 描述符、网络收发 buffer、块设备 I/O buffer、
  GPU scanout resource backing、9p 请求响应缓冲区；
- **设备返回的数据与状态**：
  VirtIO 队列完成状态、pkt 长度、mount tag、输入事件、vsock 事件；
- **VMM / 虚拟设备行为**：
  恶意或错误实现的设备可能返回畸形长度、错误 token、
  不符合协议的响应或不触发预期中断。

本模块通常不直接处理用户指针，
但会承接来自上层子系统的 buffer、request 和 handle，
因此上层和设备之间的边界必须同时纳入威胁分析。

因此威胁分析重点应覆盖：

- 设备寄存器地址或传输对象是否有效；
- DMA / virtqueue / token 与缓冲区是否始终一一对应；
- 设备返回的长度、状态和响应内容
  是否可能突破本模块边界假设；
- IRQ 路径、回收路径和正常 I/O 路径
  是否可能破坏并发或生命周期不变量。

## unsafe 代码清单

### 1. `probe_mmio_device`（`lib.rs:102`）

```rust
let transport = unsafe { MmioTransport::new(header, reg_size) }.ok()?;
```

**不变量**：`reg_base` 指向有效的 VirtIO MMIO 寄存器区域，大小至少为 `reg_size` 字节。

**为何安全**：`MmioTransport::new` 内部验证魔数和版本号，若不匹配则返回错误。
调用者（平台初始化代码）保证 MMIO 地址由固件/设备树提供。

**调用者**：
- 平台初始化代码 — 由固件/设备树保证地址有效

### 2. VirtIO GPU scanout resource backing

当前 `DisplayDevice` trait 已移除 `fb()` 及 `FrameBuffer` 类型，不再从裸 framebuffer 指针构造切片。
GPU 像素 backing 由 DRM dumb buffer（或 fbdev shadow buffer）分配，并以物理地址和长度传入
`create_scanout_resource()`。virtio-gpu 驱动仅把这段 backing attach 到 2D resource，
随后在 page flip 时提交 `TRANSFER_TO_HOST_2D`、`SET_SCANOUT` 和 `RESOURCE_FLUSH`。

**不变量**：

- backing 物理地址和长度由 DRM 层从 `GlobalPage::alloc_contiguous` 得到；
- backing 生命周期覆盖 resource attach 到 destroy 的整个区间；
- resource ID 由 DRM 层分配，destroy 时执行 detach/unref；
- virtio-gpu control queue 访问由驱动内部锁串行化。

**调用者**：
- DRM dumb-buffer 生命周期管理代码

### 3. `VirtIoNetDev::try_new()` 中的 `receive_begin`（`net.rs:216`）

```rust
let token = unsafe {
    dev.inner.lock().receive_begin(rx_buf.buffer_mut()).map_err(as_driver_error)?
};
```

**不变量**：传入的缓冲区在 `receive_begin` 返回后、`receive_complete` 调用前保持有效，
且不被其他代码修改。

**为何安全**：缓冲区由 `rx_buf` 拥有，`try_new` 结束后存储在 `rx_buffers[token]` 中，
确保生命周期覆盖整个设备使用期。

**调用者**：
- `VirtIoNetDev::try_new()` — 缓冲区存入 `rx_buffers` 数组

### 4. `VirtIoNetDev::recycle_rx()` 中的 `from_handle` 和 `receive_begin`（`net.rs:286-307`）

```rust
let mut rx_buf = unsafe { NetBuf::from_handle(rx_buf) };
let new_token = unsafe {
    self.inner.lock().receive_begin(rx_buf.buffer_mut()).map_err(as_driver_error)?
};
```

**不变量**：
- `from_handle`：传入的 handle 有效且未被其他代码使用。
- `receive_begin`：同 #3。

**为何安全**：
- `from_handle`：调用者（上层网络栈）保证 handle 由 `recv()` 返回且仅使用一次。
- `receive_begin`：缓冲区存入 `rx_buffers[new_token]`，生命周期有保障。

**调用者**：
- 上层网络栈 — 保证 handle 的唯一性

### 5. `VirtIoNetDev::recycle_tx()` 中的 `transmit_complete`（`net.rs:321-328`）

```rust
unsafe {
    self.inner.lock().transmit_complete(token, tx_buf.frame()).map_err(as_driver_error)?;
}
```

**不变量**：`token` 对应的缓冲区帧与 `tx_buf.frame()` 一致。

**为何安全**：`tx_buf` 从 `tx_buffers[token]` 取出，该位置由 `send()` 放入，
确保 token 与缓冲区的对应关系正确。

**调用者**：
- `VirtIoNetDev::recycle_tx()` — token 和缓冲区来自同一数组索引

### 6. `VirtIoNetDev::send()` 中的 `from_handle` 和 `transmit_begin`（`net.rs:337-353`）

```rust
let tx_buf = unsafe { NetBuf::from_handle(tx_buf) };
let token = unsafe {
    self.inner.lock().transmit_begin(tx_buf.frame()).map_err(as_driver_error)?
};
```

**不变量**：
- `from_handle`：handle 有效且未被使用。
- `transmit_begin`：缓冲区帧在传输完成前保持有效。

**为何安全**：
- `from_handle`：调用者保证 handle 由 `alloc_tx_buf()` 返回且仅使用一次。
- `transmit_begin`：缓冲区存入 `tx_buffers[token]`，直到 `recycle_tx()` 回收。

**调用者**：
- 上层网络栈 — 保证 handle 的唯一性

### 7. `VirtIoNetDev::recv()` 中的 `receive_complete`（`net.rs:365-372`）

```rust
let (hdr_len, pkt_len) = unsafe {
    self.inner.lock().receive_complete(token, rx_buf.buffer_mut()).map_err(as_driver_error)?
};
```

**不变量**：`token` 对应的缓冲区与 `rx_buf.buffer_mut()` 一致。

**为何安全**：`rx_buf` 从 `rx_buffers[token]` 取出，该位置由 `try_new()` 或
`recycle_rx()` 放入，确保 token 与缓冲区的对应关系正确。

**调用者**：
- `VirtIoNetDev::recv()` — token 和缓冲区来自同一数组索引

### 8. 各设备模块的 `unsafe impl Send/Sync`

```rust
unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkDev<H, T> {}
// 同理: VirtIoGpuDev, VirtIoInputDev, VirtIoNetDev, VirtIoSocketDev, VirtIo9pDev
```

**不变量**：设备类型可以安全地跨线程传递（Send）和共享引用（Sync）。

**为何安全**：
- blk / gpu / input / socket / virtio_9p：所有操作通过 `&mut self` 独占访问，
  `Sync` 仅允许共享不可变引用，不会导致数据竞争。
- net：内部使用 `SpinNoIrq` 保护共享状态，`Send`/`Sync` 安全。

## 内存安全不变量

1. **缓冲区-令牌对应**：`rx_buffers[token]` 和 `tx_buffers[token]` 中的缓冲区
   必须与 VirtIO 队列中对应 token 的缓冲区一致。违反此不变量会导致设备读写
   错误内存区域。
2. **MMIO 地址有效性**：`probe_mmio_device` 的 `reg_base` 必须指向有效的
   MMIO 寄存器区域，且在设备使用期间不被释放。
3. **NetBuf handle 唯一性**：`NetBuf::from_handle` 的调用者必须保证传入的
   handle 有效且未被重复使用。

## 线程安全

| 类型 | `Send` 条件 | `Sync` 条件 |
|------|-------------|-------------|
| `VirtIoBlkDev<H, T>` | `H: Hal, T: Transport`（手动 impl） | `H: Hal, T: Transport`（手动 impl） |
| `VirtIoGpuDev<H, T>` | `H: Hal, T: Transport`（手动 impl） | `H: Hal, T: Transport`（手动 impl） |
| `VirtIoInputDev<H, T>` | `H: Hal, T: Transport`（手动 impl） | `H: Hal, T: Transport`（手动 impl） |
| `VirtIoNetDev<H, T, QS>` | `H: Hal + 'static, T: Transport + 'static`（手动 impl） | `H: Hal + 'static, T: Transport + 'static`（手动 impl） |
| `VirtIoSocketDev<H, T>` | `H: Hal, T: Transport`（手动 impl） | `H: Hal, T: Transport`（手动 impl） |
| `VirtIo9pDev<H, T>` | `H: Hal, T: Transport`（手动 impl） | `H: Hal, T: Transport`（手动 impl） |

**说明**：所有设备类型的手动 `Send`/`Sync` impl 依赖以下保证：
- 非 net 设备通过 `&mut self` 独占访问，无内部可变性。
- net 设备通过 `SpinNoIrq` 保护共享的 `InnerDev`，中断回调和正常路径互斥。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 传入无效 MMIO 地址导致 `probe_mmio_device` 访问非法内存 | 高 | 调用者传入未映射或错误的物理地址 | 调用者（平台初始化）负责验证地址；`MmioTransport::new` 内部验证魔数 |
| T-02 | 网络设备缓冲区-令牌不匹配导致设备 DMA 到错误内存 | 高 | `rx_buffers`/`tx_buffers` 数组索引与 VirtIO 队列 token 不一致 | `try_new()` 中 `assert_eq!(token, i as u16)` 验证初始映射；`recycle_rx`/`recycle_tx` 从数组取缓冲区时检查 `is_some()` |
| T-03 | `NetBuf::from_handle` 使用无效或重复 handle | 高 | 上层网络栈重复使用已消费的 handle | 由调用者保证 handle 唯一性；`from_handle` 为 unsafe，调用者承担证明责任 |
| T-04 | scanout resource backing 悬空 | 高 | backing 在 `destroy_scanout_resource` 前被释放 | backing 由 `GlobalPage` 持有，生命周期覆盖 attach 到 destroy 全程，由 DRM/fbdev 层保证；destroy 失败会记录日志而非静默吞掉 |
| T-05 | 中断上下文与正常路径并发访问 net 设备导致数据竞争 | 中 | IRQ 回调与 `send`/`recv` 同时执行 | `InnerDev` 被 `SpinNoIrq` 包裹，所有访问通过 `lock()` 互斥 |
| T-06 | IRQ 注册失败后仍使用设备 | 中 | `register_virtio_net_irq` 返回 `ResourceBusy` | `try_new()` 传播错误，设备不会被创建 |
| T-07 | 网络设备 `free_tx_bufs` 耗尽导致发送失败 | 低 | 所有 tx 缓冲区都在飞行中 | `alloc_tx_buf` 返回 `DriverError::NoMemory`，上层应等待 `recycle_tx` 后重试 |
| T-08 | `probe_mmio_device` 传入空指针 | 高 | `reg_base` 为 null | `NonNull::new(...).unwrap()` 会 panic；调用者应确保地址非空 |
| T-09 | 恶意 VMM 通过 VirtIO 设备返回超长 `pkt_len` | 高 | `receive_complete` 返回的 `pkt_len` 超过缓冲区容量 | `recv()` 在写回 `NetBuf` 元数据前显式检查 `hdr_len + pkt_len <= capacity`，超长长度直接返回 `DriverError::InvalidInput`；`NetBuf::set_payload_len` 的 `debug_assert` 作为开发期补充检查 |
| T-10 | 恶意 VMM 返回伪造的 9p 响应 | 中 | `request()` 返回的数据不符合 9p 协议 | 上层 9p 文件系统应校验响应格式；本模块不解析内容 |
| T-11 | 恶意 VMM 返回超长 `mount_tag` | 中 | `VirtIO9p::mount_tag()` 返回超长字符串 | `virtio-drivers` 内部从配置空间读取，长度由设备配置空间字段限制 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 设备初始化失败 | VirtIO feature 协商失败、DMA 分配失败 | 设备不可用 | 对应子系统无法使用该设备 | 2 | `try_new()` 返回错误，上层可尝试其他设备或降级运行 |
| F-02 | 块设备读写失败 | 设备返回 IO 错误 | 单次读写失败 | 文件系统可能标记为只读 | 3 | 返回 `DriverError::Io`，上层文件系统处理错误 |
| F-03 | 网络设备收发丢包 | VirtIO 队列满或缓冲区不足 | 丢包 | 网络通信质量下降 | 4 | 返回 `WouldBlock`/`NoMemory`，上层协议栈重传 |
| F-04 | 网络设备 IRQ 未响应 | 中断未注册或硬件故障 | 无法及时收到数据 | 网络延迟增大或完全不可用 | 2 | 轮询模式作为降级方案；日志记录中断状态 |
| F-05 | GPU 帧缓冲区刷新失败 | 设备 flush 返回错误 | 画面不更新 | 显示异常 | 3 | 返回 `DriverError`，上层可重试 |
| F-06 | vsock 连接断开 | 对端关闭或传输错误 | 连接丢失 | vsock 通信中断 | 3 | `poll_event` 返回 `Disconnected`，上层重建连接 |
| F-07 | 9p 请求超时 | 设备无响应 | 文件操作挂起 | 文件系统不可用 | 2 | 当前无超时机制，依赖上层超时处理 |
| F-08 | `probe_mmio_device` panic | 传入空指针 | 初始化中断 | 系统启动失败 | 1 | 调用者必须验证地址有效性 |
| F-09 | 网络缓冲区 token 不匹配 | 内部状态不一致 | DMA 到错误地址 | 内存破坏 | 1 | `assert_eq!` 和 `is_some()` 检查；`BadState` 错误返回 |

## 故障管理

- **错误码**：所有设备操作通过 `as_driver_error()` 将 `virtio_drivers::Error` 映射为
  `DriverError`，上层统一处理。
- **Panic 策略**：
  - `probe_mmio_device` 中 `reg_base` 为 null 时 panic（启动阶段，不可恢复）。
  - `try_new()` 中 `receive_begin` 返回的 token 与预期不符时 panic（内部一致性错误）。
  - 其他路径不主动 panic，通过 `DriverResult` 传播错误。
- **故障恢复**：
  - 网络设备：`recycle_tx`/`recycle_rx` 回收缓冲区后可继续操作。
  - 其他设备：单次操作失败后可重试，设备状态由 `virtio-drivers` 管理。

## 隐私分析

本模块不直接处理用户数据。VirtIO 设备作为传输通道，承载块设备 I/O、网络包、
显示帧、输入事件、vsock 数据和 9p 文件请求，这些数据的隐私保护由上层子系统负责。
需确保 VirtIO 设备与 VMM 之间的通信通道不被恶意虚拟机窃听（依赖 VMM 的隔离机制）。

## 已知限制

1. **MSI-X 未启用**：x86_64 上的 MSI-X 支持代码已编写但被注释掉（TODO），
   当前所有架构使用 legacy IRQ，可能导致中断共享问题。
2. **无超时机制**：9p 设备的 `request()` 同步等待响应，无超时保护，
   恶意或故障设备可能导致调用者永久阻塞。
3. **input 设备硬编码路径**：`physical_location()` 返回固定字符串 `"virtio0/input0"`，
   不反映实际设备拓扑。

## 审计清单

修改本模块时需验证：

- [ ] 每个 `unsafe` 块均有 `SAFETY:` 注释
- [ ] 新增状态转换符合 `design.md` 中的状态机
- [ ] 新增原子操作的内存序不低于已有要求
- [ ] Drop 实现正确处理所有状态（已实现 net 设备 Drop，其他设备无需自定义 Drop）
- [ ] 新增 panic 路径有对应的 PanicGuard 或等效保护
