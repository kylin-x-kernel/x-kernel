# block — 安全与可靠性分析

## 信任边界

driver 报告的 disk identity、容量、block size 和 I/O completion 跨越 block-core 边界。
外部介质内容不可信，格式验证属于使用该设备的文件系统。

## 不变量

- major 必须非零，minor range 非空且不能溢出或与已发布 disk 重叠；
- block size 必须非零，`num_blocks * block_size` 在 `u64` 中可表示；
- 一个 `dev_t` 在 resident registry 中只对应一个 `BlockDevice`；
- 一个 canonical `BlockDevice` 同时至多有一个 exclusive holder，claim token 负责释放；
- 每个 I/O buffer 长度是 block size 的整数倍，完整 extent 位于当前 capacity 内；
- block offset 的加法必须 checked；
- operations object、`Gendisk` 和 `BlockDevice` 都是 `Send + Sync`；
- registry 锁内不执行 driver I/O 或 callback。
- backend 固有只读能力由 `Gendisk` 在发布前保存且不可被管理接口清除；
- 管理只读状态只由 owning `Gendisk` 保存，有效只读状态为两者的并集；
- 所有 `BlockDevice` 写入在 backend 前检查有效只读状态。

## unsafe

`drivers/contracts/block/src/lib.rs` 没有 `unsafe` block。具体硬件 backend 的 MMIO/DMA 安全边界由
各 backend 文档负责。

## 故障处理

| 故障 | 结果 |
|---|---|
| identity/range 冲突 | `AlreadyExists`，不发布半成品 |
| 已有 exclusive holder | `ResourceBusy`，不建立第二个 holder |
| 无效或越界 I/O | `InvalidInput`，不调用 backend |
| backend I/O/flush 失败 | 原样传播 `DriverError` |
| read-only disk 写入 | `ReadOnly`；KVFS 映射为 Linux `EPERM` |
| 对固有只读 disk 执行 `BLKROSET 0` | `ReadOnlyFilesystem`（Linux `EROFS`），状态保持只读 |
| 未知 disk-specific ioctl | `NotATty` |
| disk 撤销后新 open/mount | canonical lookup 失败 |

## 已知限制

当前没有 partition scan，只发布 whole-disk part0。热移除不会主动冻结已有 mounted
filesystem；已有引用的后续 I/O 依赖 backend 返回设备错误。
