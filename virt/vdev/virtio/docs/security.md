# vdev-virtio — 安全与可靠性分析

## 信任模型

- 调用方（设备实现 / VMM）是受信任的：它们选择队列参数、传入 `GuestDma`
  能力，并决定何时处理队列。
- 访客是**不受信任**的：访客可自由写 MMIO 寄存器、构造 virtqueue 描述符与
  available/used ring、指定任意访客物理地址与长度。
- 本 crate 必须在访客可控输入上做边界与一致性校验，任何越界、错宽、非法
  状态都返回错误或安全降级，而不是 UB。

## 外部边界 / 攻击面

| 边界 | 说明 |
|------|------|
| MMIO 寄存器写 | 访客写入的 offset/width/value；offset 可能落在标准寄存器或设备配置区 |
| MMIO 寄存器读 | 访客读宽度；无效宽度必须被拒绝 |
| 队列地址 | `QUEUE_DESC/AVAIL/USED` 高低位写入的访客物理地址 |
| 队列大小 | `QUEUE_NUM` 与设备 `max_size` 的关系 |
| 描述符链 | 链长度、`NEXT`/`INDIRECT` 标志、地址与长度，来自访客内存 |
| 环索引 | available/used idx 为访客写入的自由整数，存在回绕 |
| 设备配置区 | 设备实现解释的配置偏移，来自访客读 |

本 crate 不接触 **user memory / 文件系统 / 网络数据**；它只接触上述
**MMIO/虚拟队列/DMA** 边界。

## unsafe 代码清单

全部 `unsafe` 集中在 `src/memory.rs`，服务于 `GuestMemoryAccessor` 的默认实现；
生产路径使用 `dma::DmaGuestMemory`（无 `unsafe`）。

| 位置 | 不变量 | 为什么安全 | 约束入口 |
|------|--------|-----------|----------|
| `memory.rs:66` `read_buffer` | `translate_and_get_limit` 返回的主机地址对至少 `limit` 字节可解引用，且 `limit >= data.len()` | 长度在拷贝前已校验；`copy_nonoverlapping` 的源区间与目标 `data` 不重叠 | 仅 `GuestMemoryAccessor` 实现者调用 |
| `memory.rs:89` `write_buffer` | 同上（目标侧） | 长度已校验；目标为访客内存映射 | 同上 |
| `memory.rs:110` `read_obj`（`unsafe fn`） | `T` 为可按字节寻址、无非法位模式/填充不变量/自有指针的 POD；`MaybeUninit<T>` 的字节视图有效 | `read_buffer` 填满全部字节后才 `assume_init` | `unsafe fn` 的 `# Safety` 契约，调用方保证 `T` 布局 |
| `memory.rs:132` `write_obj`（`unsafe fn`） | `T` 为 POD | 读取活动值字节对 POD 安全 | 同上 |

`read_obj`/`write_obj` 已声明为 `unsafe fn`：`T: Sized` 不保证是 POD，若传入含
指针、无效位模式或 `bool` 的类型会触发 UB，因此把 POD 约束作为调用方必须满足的
`# Safety` 契约，而不是默认安全方法。

调用方需满足：`translate_and_get_limit` 返回的宿主地址必须可直接解引用，且
返回的 `limit` 必须真实反映连续可访问范围。

## 内存安全不变量

1. 任何来自访客的长度/地址在用于读写前都必须经过翻译与上限校验。
2. 描述符链遍历必须有界（`MAX_DESCRIPTOR_CHAIN_LENGTH`），并防止环回/自引用
   导致的无限循环。
3. 环索引回绕必须使用 `wrapping` 语义，且取模到队列 size。
4. `VirtioQueue` 在 `ready` 之前不得被数据路径访问；`ready` 写入前必须完成环
   布局（区间不重叠、不越界）校验。

## 线程安全

- `vdev_core::GuestDma: Send + Sync`；`DmaGuestMemory` 仅持共享引用，天然
  `Send`。
- `VirtioMmioState` 的所有可变状态由 `SpinNoIrq` 保护，可在访客退出路径上
  并发访问；`T: GuestMemoryAccessor + Clone` 以 `Arc<T>` 共享。
- 本 crate 不注册中断、不创建任务，线程安全责任在调用方对设备实例的共享方式。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 越界访客地址导致宿主内存越界读写 | 高 | 描述符/环地址指向未映射区 | 所有读写经 `GuestDma` 翻译，失败返回 `DmaError`→`VirtioError::InvalidAddress`；`GuestMemoryAccessor` 实现必须校验 `limit` |
| T-02 | `QUEUE_READY` 环区间重叠/越界 | 高 | 访客提交非法 desc/avail/used 布局 | ready 前做布局校验，非法则拒绝置 ready |
| T-03 | 恶意描述符链导致无限循环/耗尽 | 中 | 描述符自引用/环回 | 链长度上限 `MAX_DESCRIPTOR_CHAIN_LENGTH`，超过报错 |
| T-04 | 非法 feature 请求 | 中 | 驱动写 `FEATURES_OK` 但包含未广告位 | 校验子集关系，不满足则清 `FEATURES_OK` |
| T-05 | 错宽 MMIO 访问 | 低 | 访客用非 1/2/4/8 字节宽度 | `transport` 拒绝并返回错误/0 |
| T-06 | 中断漏报 | 低 | 驱动读 `INTERRUPT_STATUS` 后新事件到达 | `raised_after_read` 标记并返回 `InterruptPending`，由设备再次注入 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 队列处理失败 | 访客地址非法/描述符非法 | 单次请求报错 | 访客 I/O 失败，可重试 | 3 | 返回 `VirtioError`，设备实现回写错误状态 |
| F-02 | 布局校验拒绝 ready | 访客参数非法 | 队列不可用 | 访客驱动初始化失败 | 3 | 保持未就绪，访客可重新编程 |
| F-03 | 后端可能需要延迟处理 | 设备后端会阻塞 | `QueuePending` | 若 VMM 无轮询路径则队列停滞 | 3 | 设备实现声明 `requires_deferred_processing`，由 VMM 提供轮询 |
| F-04 | 中断状态不一致 | 并发读写中断寄存器 | 中断漏报/重复 | 访客可能等待超时 | 4 | 单锁保护 + `raised_after_read` |

## 故障管理

- 所有访客可控失败路径返回 `VirtioError`（`InvalidAddress`、`InvalidQueue`、
  `InvalidDescriptor`、`InvalidBufferSize` 等），不 panic。
- `VirtioMmioState::new` 假定调用方已校验队列 size；越界索引在运行期返回
  `None`/错误而非 panic。
- 无锁 poisoning 问题：使用 `SpinNoIrq`，不涉及 OS 线程 poison。

## 隐私分析

本 crate 不处理用户隐私数据；它只搬运访客内存字节，字节内容由设备实现解释。
无日志打印访客数据（除设备实现自身按需 trace）。

## 已知限制

- `ut-tests/` 的 4 个回归用例在内核 harness 中执行，覆盖固定 mock 输入与布局，
  不覆盖真实 VM/DMA 隔离。descriptor fixture 的原始指针仅指向本例存活且不扩容的
  Vec；其 translator 返回 `usize::MAX`，只能用于现有固定范围输入，不能用来证明
  任意地址的边界安全。该测试约束在迁移中保持不变。
- 仅实现 VirtIO MMIO 传输（version 2），不支持 PCI 传输。
- event-index 尚未实现：传输不广告 `VIRTIO_F_RING_EVENT_IDX`，队列的
  `event_idx_enabled` 恒为 `false`。布局计算仍按包含 event-index footer 处理，
  以保证协商无关的环区间校验。
- indirect 描述符的完整数据路径能力取决于上层设备实现。
- 不负责中断注入与硬件交互，由设备实现/VMM 完成。

## 审计清单

- [ ] 任何新的访客内存读写是否都经过翻译与长度校验？
- [ ] 新描述符处理是否保持链长度上限与回绕安全？
- [ ] `QUEUE_READY` 前是否仍执行完整的环布局校验？
- [ ] `unsafe` 块 / `unsafe fn` 是否有 `SAFETY:`/`# Safety` 说明并保持
      `memory.rs` 的翻译与 POD 契约？
- [ ] MMIO 访问宽度校验是否覆盖新增寄存器？
- [ ] 新增状态是否有对应锁保护，避免数据竞争？
