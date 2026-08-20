# fs_block — 设计文档

## 定位

`fs_block` 不拥有 block identity、`dev_t` registry 或 VFS block-special file
operations。那些职责分别属于 `drivers/block` 与 KVFS，和 Linux 的 block core / VFS
层次一致。

本 crate 只保留 `SeekableDisk`：把 canonical `block::BlockDevice` 适配为带字节游标的
介质。root filesystem 类型选择不属于 block I/O adapter；所选实现直接向 KVFS 暴露
canonical `FileSystemType` 静态对象。

## 架构

```text
driver operations -> block::Gendisk -> block::BlockDevice
                                           |
                                           +--> KVFS def_blk_fops
                                           +--> KVFS get_tree_bdev
                                           `--> fs_block::SeekableDisk
```

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
- 本 crate 不定义 filesystem type、root provider 或 mount callback；root 与用户挂载都由
  KVFS canonical registry 分派。
