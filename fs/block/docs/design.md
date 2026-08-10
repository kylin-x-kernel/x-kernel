# fs_block — 设计文档

## 定位

`fs_block` 不拥有 block identity、`dev_t` registry 或 VFS block-special file
operations。那些职责分别属于 `drivers/block` 与 KVFS，和 Linux 的 block core / VFS
层次一致。

本 crate 只保留两个文件系统侧适配：

- `SeekableDisk`：把 canonical `block::BlockDevice` 适配为带字节游标的介质；
- `RootFileSystem`：由 Kconfig 在链接期选择唯一 root `FileSystemType`，不提供另一条
  mount 算法。

## 架构

```text
driver operations -> block::Gendisk -> block::BlockDevice
                                           |
                                           +--> KVFS def_blk_fops
                                           +--> KVFS get_tree_bdev
                                           `--> fs_block::SeekableDisk

Kconfig root choice -> RootFileSystem::file_system_type
                              |
                              `--> FsContext -> the same get_tree callback as mount(2)
```

`RootFileSystem` 只返回 filesystem type descriptor。early boot 先建立 bootstrap rootfs 和
devtmpfs，因此 root device 也能按 `/dev/<name>` 解析，并走普通
`FsContext -> get_tree_bdev -> fill_super`。不存在 root 专用 `mount_bdev` callback。

## SeekableDisk 算法

完整 block 直接提交给 `BlockDeviceOperations`。首尾非对齐访问使用一个 read buffer 和
一个 write buffer；改变游标、读取或显式 flush 前先提交 pending partial write，保持写后
读顺序。游标状态由独占 `&mut self` 保护，共享介质并发由文件系统负责。

普通 drop 不隐式吞掉 flush 错误；文件系统必须在 sync/unmount 路径显式调用 `flush()`。

## 设计约束

- 本 crate 不建立设备表，也不把名称反解为 major/minor。
- device-backed mount source 的 pathname、`nodev` 和 `rdev` 校验属于 KVFS
  `get_tree_bdev`。
- `Gendisk`、`BlockDevice`、容量和设备移除生命周期属于 block core。
- root filesystem 的实现选择只有链接期 provider 和 KVFS `FileSystemType` 这一份事实。
