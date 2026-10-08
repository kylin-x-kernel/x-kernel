# block — 安全与可靠性分析

## Scope

This analysis covers the block-core contract in `src/lib.rs` and
`src/completion.rs`. The concrete backends (`src/ahci.rs`,
`src/bcm2835sdhci.rs`, `src/sdmmc.rs`, `src/ramdisk*.rs`) are hardware
adapters excluded from this document and audited separately; the
registry, validation, and completion invariants below are the core's
own.


## 信任边界

driver 报告的 disk identity、初始容量、logical block size 和 I/O completion 跨越
block-core 边界。
外部介质内容不可信，格式验证属于使用该设备的文件系统。

## 不变量

- major 必须非零，minor range 非空且不能溢出或与已发布 disk 重叠；
- logical block size 必须在 512..=4096 bytes 内且为 2 的幂；
- `Gendisk` 构造时即关联唯一 `part0` 和 `RequestQueue`，发布不能替换它们；
- `GendiskRef` 经 part0 保活磁盘，`BlockDevice` 强持有完整 `Gendisk`；内部 disk Arc 不外露；
- 磁盘只弱引用 part0，最后一个外部拥有引用释放后设备、磁盘和 backend 可完整回收；
- 每个可借用的磁盘必有 live part0，`disk()` 不返回缺失状态，`part0()` 的 upgrade 必须成功；
- `BlockDevice` 的 start/capacity 使用 512-byte sector，`num_sectors * 512` 在 `u64` 中可表示；
- 一个 `dev_t` 在 resident registry 中只对应一个 `BlockDevice`；
- 一个 canonical `BlockDevice` 同时至多有一个 exclusive holder，claim token 负责释放；
- 每个 I/O buffer 长度是 logical block size 的整数倍，完整 sector extent 位于当前 capacity 内；
- block/sector 换算以及 offset/length 的乘加必须 checked；
- operations object、`Gendisk` 和 `BlockDevice` 都是 `Send + Sync`；
- registry 锁内不执行 driver I/O 或 callback。
- backend 固有只读能力由 `Gendisk` 在发布前保存且不可被管理接口清除；
- 管理只读状态只由 owning `Gendisk` 保存，有效只读状态为两者的并集；
- 所有 `BlockDevice` 写入在 backend 前检查有效只读状态。
- `Gendisk` 不实现 driver operations trait；`BlockDevice` 的阶段性同步 trait 实现只委托
  canonical 方法，不能绕过容量、边界和只读检查。

## unsafe

`drivers/contracts/block/src/lib.rs` 没有 `unsafe` block。具体硬件 backend 的 MMIO/DMA 安全边界由
各 backend 文档负责。

## 故障处理

`BlockCompletionOperations::process_completed_requests` 是可信驱动回调，必须有限且不睡眠。host 的串行执行
保证以单一注册为边界，不能为同一目标创建多个注册后假设仍然互斥。完成回调不得
在持有设备锁时唤醒调用者；目标强引用必须覆盖同步停止，之后才能销毁设备资源。

完成等待契约要求 provider 的 signals 只拥有通知状态，不拥有请求或 DMA 数据。
调用方在设备/registry 锁外通知；`BlockWaiter` 仅由准备它的任务在可睡眠上下文使用。
准入等待失败由事务层取消 pending 节点并传递准入机会；终态等待无可恢复错误出口，
避免尚在 DMA 中的请求随等待错误提前释放。契约本身不实现队列或 DMA 生命周期，
也不验证设备已经完成；驱动必须先发布真实结果再发终态通知。

| 故障 | 结果 |
|---|---|
| identity/range 冲突 | `AlreadyExists`，不发布半成品 |
| logical block size 或初始 sector capacity 无效 | `InvalidInput`，不构造半成品 |
| 已有 exclusive holder | `ResourceBusy`，不建立第二个 holder |
| 无效、未对齐或越界 I/O | `InvalidInput`，不调用 backend |
| backend I/O/flush 失败 | 原样传播 `DriverError` |
| read-only disk 写入 | `ReadOnly`；KVFS 映射为 Linux `EPERM` |
| 对固有只读 disk 执行 `BLKROSET 0` | `ReadOnlyFilesystem`（Linux `EROFS`），状态保持只读 |
| 未知 disk-specific ioctl | `NotATty` |
| disk 撤销后新 open/mount | canonical lookup 失败 |

## 已知限制

当前没有 partition scan，只发布 whole-disk part0。热移除不会主动冻结已有 mounted
filesystem；已有引用的后续 I/O 依赖 backend 返回设备错误。
