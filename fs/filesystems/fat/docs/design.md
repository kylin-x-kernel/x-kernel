# fat — 设计文档

## 定位

`fat` 把 `axfatfs` 的 FAT 介质对象接入 KVFS。它拥有 FAT mount state、目录/文件 inode
adapter 和 `fs_block::SeekableDisk` 字节游标，不拥有 block-device registry、mount
namespace policy 或 root 专用 provider。

## 范围

- `src/lib.rs`：canonical `FileSystemType`、`get_tree` 与受锁保护的 FAT handle wrapper；
- `src/fs.rs`：`fill_super`、mount state 与 superblock operations；
- `src/dir.rs`、`src/file.rs`：KVFS inode/file operations；
- `src/util.rs`：metadata、时间与错误转换。

## 架构

```text
KVFS FileSystemType registry
        -> FsContext -> get_tree_bdev
        -> FatFilesystem::fill_super
        -> fs_block::SeekableDisk -> block::BlockDevice
        -> SuperBlock / VfsInode / FAT handles
```

本 crate 只拥有一个静态 `FILE_SYSTEM_TYPE`，并由自己的 `register_init` 回调将它注册进 KVFS。root boot
与用户 `mount(2)` 都经该对象进入 `get_tree_bdev -> fill_super`，不存在 root 专用名称、
能力字段或 mount callback。

## 调用约束 / 执行上下文

挂载和文件操作可能分配内存、取得 sleepable mutex 并执行 block I/O，只能在调度器与
block subsystem 已就绪的可阻塞任务上下文运行，不能从 IRQ context 调用。KVFS 在进入
`fill_super` 前已经完成 source pathname、block-special inode、`nodev`、`rdev` 和介质只读
策略校验。

## 算法流程

1. `FileSystemType::get_tree` 调用 KVFS `get_tree_bdev` 取得 canonical `BlockDevice`，并由
   VFS registry 按 `(s_type, dev_t)` 查找或 reservation；已有实例不再调用 FAT。
2. VFS 分配带有 `s_type/s_bdev/s_flags` 的 nascent `SuperBlock` 后才调用 `fill_super`；FAT
   从该对象取得设备并由 `SeekableDisk` 解析格式，只安装私有 operations 与 root，不接收或
   复制 identity 参数；格式或 I/O 错误作为 `VfsResult` 返回，不 panic。
3. mount state 固定地址并由 mutex 串行化 FAT handle 操作。
4. 建立 KVFS `SuperBlock`、root inode 和 root dentry 后才向调用方返回可挂载对象。

## 并发模型

一个 `Mutex<FatFilesystemInner>` 串行化 `axfatfs` 和 self-referential handle 访问。
`FsRef` 记录 owner 地址，每次借用都校验 matching guard；它不建立第二套 inode cache。

## 设计决策

- 类型 identity 只由 KVFS registry 中的静态 `FILE_SYSTEM_TYPE` 表达。
- mount instance identity 与 RO/RW 一致性由 KVFS superblock registry 拥有，FAT 不维护
  第二套设备实例 cache。
- block source policy 属于 KVFS，字节游标属于 `fs_block`，FAT 格式状态属于本 crate。
- 初始化失败返回错误，使 root candidate 和普通 mount 使用同一失败语义。

## Drop / 资源释放

`Arc<SuperBlock>`/`Arc<FatFilesystem>` 管理 mount state 生命周期。文件和目录 wrapper 只在
owner mutex 下访问内部 FAT handle；`SeekableDisk` 的持久化错误必须由显式 sync/flush 路径
传播，不能依赖 drop 吞掉错误。
