# vdev-virtio-blk — 安全与可靠性分析

## 信任模型

- 调用方（VMM 组装代码）受信任：提供 `GuestDma`、`IrqSender` 与容量。
- 访客不受信任：控制 MMIO 寄存器与 virtqueue 内容，包括请求头中的
  `request_type`、`sector`，以及描述符地址与长度。
- 本 crate 必须保证：恶意请求最多导致该请求以 IOERR 完成，不得越界访问宿主
  内存、不得 panic、不得破坏设备/VM 状态。

## 外部边界 / 攻击面

| 边界 | 说明 |
|------|------|
| MMIO 标准寄存器 | 来自 `vdev-virtio` 的传输状态机（含 `QUEUE_READY` 布局校验） |
| 配置空间读 | 段大小、几何等常量，只读 |
| 请求头描述符 | 访客提供的 `VIRTIO_BLK_T_*`、`sector`、描述符方向与长度 |
| 数据描述符链 | 访客提供的地址/长度，经 `GuestDma` 读写 |
| status 描述符 | 设备写入 1 字节状态 |
| `QUEUE_NOTIFY` | 触发队列处理 |
| 后端 | `BlockBackend` 实现（默认 RAM 盘） |

## unsafe 代码清单

本 crate 无 `unsafe` 代码。所有访客内存访问经 `vdev-virtio::DmaGuestMemory`
→ `vdev_core::GuestDma`，`unsafe` 仅在 `kvmm` 的 `GuestDma` 实现与
`vdev-virtio::memory` 中（见各自文档）。

## 内存安全不变量

1. 数据请求必须通过 `validate_data_request`：段数 ≤ `seg_max`、单段 ≤
   `size_max`、总长按 `SECTOR_SIZE` 对齐、`[sector, sector+len)` 落在容量内。
2. 描述符方向必须匹配请求类型：`IN` 要求数据描述符可写，`OUT` 要求可读。
3. 请求头描述符必须可读且长度 ≥ `VirtioBlockHeader::SIZE`；status 描述符必须
   可写且长度 ≥ 1。
4. 缓冲区分配使用 `try_reserve_exact`，分配失败返回
   `VirtioError::InvalidBufferSize`，不对访客声明长度直接 `resize` 造成 OOM panic。
5. 所有 `GuestDma` 访问失败都映射为 I/O 错误，不传播为 UB。

## 线程安全

- 设备实例作为 `Box<dyn MmioDevice>` 由 VMM MMIO 总线以 `&mut self` 串行访问；
  内部共享状态由 `SpinNoIrq` 保护。
- `IrqSender` 为 `Send + Sync`；注入在设备锁外调用。
- `RamDiskBackend` 的内存盘由 `kspin::SpinNoIrq` 保护，可在多 vCPU 触发下安全访问。
  后端 I/O 运行在队列 `SpinNoIrq` 锁内（本地中断屏蔽），故后端锁必须不可睡眠，
  避免在中断屏蔽状态下阻塞导致死锁。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 超容量/越界读写请求 | 高 | 访客给出越界 `sector`/长度 | `validate_data_request` + `RamDiskBackend::checked_range` 双重校验，失败 IOERR |
| T-02 | 描述符方向伪造 | 中 | `IN` 请求的数据描述符不可写等 | `process_read`/`process_write` 校验方向，失败 `InvalidDescriptor` |
| T-03 | 超大长度导致内存耗尽 | 中 | 访客声明巨大数据长度 | 请求长度必须通过描述符与 `seg_max`/`size_max` 约束；缓冲区用 `try_reserve_exact` |
| T-04 | 请求头/status 描述符非法 | 中 | 链长不足、头可写、status 不可写 | 处理前校验链长与方向 |
| T-05 | 复位后处理失效的挂起请求 | 中 | 后端延迟期间访客复位 | 复位清除 `pending_head` |
| T-06 | 延迟后端无轮询导致队列停滞 | 低 | `requires_deferred_processing` 为真但 VMM 无轮询 | `deferred_pending` 记录并告警，需 VMM 提供轮询 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 请求以 IOERR 完成 | 非法 range/方向/后端错误 | 单请求失败 | 访客 I/O 报错，可重试 | 3 | 回写 `VIRTIO_BLK_S_IOERR`，继续处理后续请求 |
| F-02 | 请求挂起 | 后端 `WouldBlock` | 该 head 存入 `pending_head` | 队列暂缓，待继续处理 | 3 | 返回 `QueuePending`，由 `process_pending_queue` 继续 |
| F-03 | 缓冲区分配失败 | 内存不足 | 请求 IOERR | 访客 I/O 失败 | 3 | `try_reserve_exact` 失败返回错误，不 panic |
| F-04 | 未支持请求 | 未知 `request_type` | 请求以 UNSUPP 完成 | 访客可见不支持 | 4 | 回写 `VIRTIO_BLK_S_UNSUPP` |
| F-05 | 构造 panic | 容量非 512 倍数 | 启动期 panic | 设备无法创建 | 2 | `RamDiskBackend::new` 断言配置错误（调用方受信任的配置失误） |

## 故障管理

- 访客可控失败一律通过 `VirtioResult` 与状态码返回，不 panic。
- 唯一的 panic 点在 `RamDiskBackend::new`：容量必须是 512 的整数倍，属调用方
  配置错误（受信任输入），非访客可控。
- 后端 `read`/`write` 长度不匹配返回 `BackendError`，请求以 IOERR 完成。

## 隐私分析

本 crate 不处理用户隐私数据；它只在访客 RAM 盘与访客描述符缓冲间搬运字节。
`selftest` 仅使用固定测试数据，无真实用户数据。

## 已知限制

- 默认后端为内存 RAM 盘，非持久化。
- 未实现 `VIRTIO_BLK_F_*` 之外的高级特性（discard/write-zeroes 等）。
- 仅 MMIO 传输，不支持 PCI。
- 默认 base/IRQ 固定为 QEMU `virt` slot 1，多设备/自定义布局需扩展构造参数。

## 审计清单

- [ ] 新增请求类型是否都经过方向与容量校验？
- [ ] 所有访客长度是否在分配前受 `seg_max`/`size_max`/容量约束？
- [ ] 缓冲区分配是否保持 `try_reserve_exact` 失败即返回错误？
- [ ] 复位路径是否清除所有挂起的中间状态？
- [ ] 新增后端是否声明正确的 `requires_deferred_processing` 并提供轮询路径？
- [ ] 中断注入是否在设备锁之外调用？