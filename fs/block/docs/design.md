# fs_block — 设计文档

## 定位

`fs_block` 是内核 block class 与文件系统实现之间的适配层。它提供两项能力：

- `SeekableDisk`：把按 block 访问的 `ClassDevice<BlockDeviceImpl>` 适配成带字节游标的设备；
- `FileSystemType`：挂载 Kconfig 所选 block root filesystem 的链接期单实现接口。

## 范围

- `src/lib.rs`：block I/O 游标、缓冲和文件系统类型接口。

## 架构

```text
fs/Kconfig choice
        |
        v
kfeat / kruntime links exactly one provider
        |
        +-- kext4_vfs
        +-- rsext4_vfs
        `-- fat
                |
                v
fs_boot -- FileSystemType::mount_bdev --> KVFS SuperBlock

filesystem library -- SeekableDisk --> kclass block device
```

`FileSystemType` 对应当前 X-Kernel 所需的 Linux `struct file_system_type`
block-mount 能力。Kconfig 的 root filesystem choice 保证每个镜像只有一个 provider，
因此这里不维护第二份 name、feature 或运行时 registry。缺少 provider 或同时链接多个
provider 都会在最终链接阶段失败。

如果未来允许同一镜像同时挂载多个 block filesystem type，应在 VFS 层引入真正的
Linux 式 filesystem-type 注册和按名称查找；不应在 boot 中恢复按文件系统名称分支。

## 调用约束 / 执行上下文

`FileSystemType::mount_bdev` 在文件系统初始化期间的普通任务上下文调用，允许分配、
加锁和执行设备 I/O，不可在中断上下文调用。调用前 block class、分配器和 KVFS 必须
已经可用。

`SeekableDisk` 由具体文件系统持有。读写和 `set_position` 可能访问设备并阻塞；调用者
负责使用文件系统自己的锁串行化同一实例。

## 算法流程

root mount：

1. `fs_boot` 按 root block 配置选择 `ClassDevice`。
2. 调用 `FileSystemType::mount_bdev`，链接器把调用解析到 Kconfig 所选 provider。
3. provider 校验并挂载介质，返回具有自身文件系统名称的 KVFS `SuperBlock`。
4. `fs_boot` 只负责把该 superblock 安装为初始 mount namespace 根。

`SeekableDisk` 对齐设备 block size 处理完整 block I/O；首尾非对齐部分使用内部缓冲。
游标跳转和读取前会先 flush 待写的 partial block，保持读后写顺序。

## 并发模型

`fs_block` 不维护全局可变 registry。`FileSystemType` provider 在链接期确定。
`SeekableDisk` 需要 `&mut self` 执行游标和缓冲操作，共享时由上层文件系统锁保护。

## 设计决策

- Kconfig 是 root filesystem 选择的唯一配置源；Cargo feature 只完成 crate 链接。
- boot 不依赖具体 ext4/FAT crate，也不读取 `KFEAT_FS_*` 决定控制流。
- 接口放在 `fs_block` 而非 `kvfs`，避免 KVFS 反向依赖具体 block device class。

## Drop / 资源释放

`ClassDevice` 由引用计数句柄持有。`SeekableDisk` 的普通 drop 不隐式报告 flush 错误，
文件系统必须在同步或卸载路径显式调用 `flush()`。
