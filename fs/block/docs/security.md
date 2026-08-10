# fs_block — 安全与可靠性分析

## 信任边界

`SeekableDisk` 接收已经由 block core 建立的 `Arc<BlockDevice>`。介质内容、容量、block
size 和 I/O 结果仍不可信；磁盘格式、feature 和 checksum 由具体文件系统校验。

本 crate 不访问用户指针、MMIO、PIO 或 DMA，也不拥有设备发布和 pathname lookup。

## 内存与并发不变量

`fs/block/src` 没有 `unsafe` block。

- partial-block buffer 长度等于 block size；block size 必须为二次幂；
- `offset` 始终小于 block size，跨 block 后归零；
- 可变游标操作要求独占 `&mut SeekableDisk`；
- pending write 只在显式可传播错误的路径提交；
- 最终镜像恰好链接一个 `RootFileSystem` provider，provider 只返回 canonical
  `FileSystemType`。

## 故障处理

| 故障 | 结果 |
|---|---|
| block read/write/flush 失败 | 原样传播给 filesystem adapter |
| root provider 缺失或重复 | `kiface` 链接约束使镜像构建失败 |
| 非对齐 partial write 的预读失败 | 不修改 write buffer dirty 状态并返回错误 |

## 审计清单

- 是否误把设备 registry 或 `dev_t` identity 放回本 crate？
- 新 adapter 是否在 drop 中吞掉持久化错误？
- root provider 是否增加了绕过 `FsContext` 的 mount 入口？
