# fs_block — 安全与可靠性分析

## 信任模型

block device 内容和设备 I/O 结果不可信。`fs_block` 只提供访问适配；磁盘格式、范围、
feature 和 checksum 校验由所选文件系统 provider 负责。

## 外部边界 / 攻击面

- `ClassDevice<BlockDeviceImpl>` 可能报告异常 block size、容量或 I/O 错误。
- `RootFileSystem::mount_bdev` 进入解析不可信磁盘元数据的具体文件系统。
- `SeekableDisk` 的 position 和长度运算必须保持在设备实现可处理的范围内。

本 crate 不直接访问用户指针、MMIO、PIO 或 DMA buffer。

## unsafe 代码清单

`fs/block/src` 没有 `unsafe` block。内存安全依赖 Rust 所有权、切片边界检查和
`ClassDevice` 的安全接口。

## 内存安全不变量

- partial-block 缓冲区长度始终等于设备 block size。
- `offset` 始终小于 block size；到达 block 末尾时归零并递增 block id。
- mutable cursor 操作需要独占 `&mut SeekableDisk`。
- 最终镜像必须恰好提供一个 `RootFileSystem` 实现。
- provider 报告的 canonical name 必须与其返回的 superblock 类型一致，并在整个内核
  生命周期内有效。

## 线程安全

`RootFileSystem` 没有运行时可变注册状态。`SeekableDisk` 不提供内部并发控制；具体
文件系统必须在共享访问时持有其设备或文件系统锁。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 恶意磁盘元数据导致越界或错误挂载 | 高 | provider 未校验磁盘输入 | 接口传播 `VfsResult`；格式校验由具体 provider 承担 |
| T-02 | boot 与 Kconfig 选择不同文件系统实现 | 中 | 维护重复运行时分支 | boot 只调用 exactly-one `RootFileSystem`；Kconfig/Cargo 决定 provider |
| T-03 | partial write 未持久化 | 中 | 上层未在同步路径调用 `flush` | API 暴露显式 `flush`；文件系统同步/卸载负责调用 |
| T-04 | block type 注册名与实际 superblock 不一致 | 中 | provider 返回错误 canonical name | `RootFileSystem` provider 同时拥有 `name` 和 `mount_bdev` 实现；boot 不改写名称 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | root mount 失败 | 格式错误、不支持 feature 或设备 I/O 错误 | provider 返回错误 | boot 无法建立根目录 | 1 | 错误传播给 `fs_boot`，由 boot 输出原因并停止启动 |
| F-02 | provider 缺失或重复 | Cargo feature 接线错误 | 最终符号无法解析或重复 | 构建失败 | 2 | `kiface` exactly-one 链接约束 |
| F-03 | partial-block flush 失败 | 设备写入失败 | 缓冲数据未持久化 | 文件系统同步失败 | 2 | `flush` 返回 `DriverResult`，禁止吞掉错误 |

## 故障管理

文件系统 mount 错误通过 `VfsResult` 传播；设备游标错误通过 `DriverResult` 传播。
本 crate 不记录重复日志，具体文件系统和 boot 层按各自语义记录上下文。

## 已知限制

当前 Kconfig 每个镜像只选择一个 block root filesystem provider，因此接口是 exactly-one。
同镜像动态注册多个 block filesystem type 尚未实现。

## 审计清单

- 新 root filesystem 是否只提供 `RootFileSystem`，而未在 boot 增加名称分支？
- `RootFileSystem::name()` 是否是 canonical name，并与返回的 superblock 一致？
- provider 是否完整校验不可信磁盘输入并传播 mount 错误？
- `SeekableDisk` 用户是否在持久化边界显式 flush？
- 是否意外启用了两个 `RootFileSystem` provider？
