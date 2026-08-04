# fs_block — 设计文档

## 定位

`fs_block` 是内核 block class 与文件系统实现之间的适配层。它提供两项能力：

- `SeekableDisk`：把按 block 访问的 `ClassDevice<BlockDeviceImpl>` 适配成带字节游标的设备；
- `RootFileSystem`：报告 canonical type name，并挂载 Kconfig 所选 block root
  filesystem 的链接期单实现接口。

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
        `-- fat
                |
                v
fs_boot -- RootFileSystem::name -------> KVFS FileSystemType registry
        `- RootFileSystem::mount_bdev -> KVFS SuperBlock

filesystem library -- SeekableDisk --> kclass block device
```

`RootFileSystem` 不是第二个 Linux `struct file_system_type`；它只隔离 KVFS 与
`kclass::BlockDeviceImpl`，表达当前 Kconfig exactly-one root provider。真正的 Linux
filesystem-type 对象和 registry 位于 KVFS。provider 报告的 canonical name 由
`fs_boot` 登记到该注册表；缺少 provider 或同时链接多个 provider 仍会在最终链接阶段失败。
Linux root mount 会按 `rootfstype=` 或注册表中的 `FS_REQUIRES_DEV` 类型进入通用
`path_mount()` / `get_fs_type()` / `get_tree_bdev()` 链路；当前 adapter 仍是
X-Kernel 在 root namespace 建立前直接持有 `ClassDevice` 的已记录差异。

## 调用约束 / 执行上下文

`RootFileSystem::name` 是无副作用的静态查询。`mount_bdev` 在文件系统初始化期间的
普通任务上下文调用，允许分配、加锁和执行设备 I/O，不可在中断上下文调用。调用前
block class、分配器和 KVFS 必须已经可用。

`SeekableDisk` 由具体文件系统持有。读写和 `set_position` 可能访问设备并阻塞；调用者
负责使用文件系统自己的锁串行化同一实例。

## 算法流程

root mount：

1. `fs_boot` 调用 `RootFileSystem::name`，把所选 block type 登记到 KVFS registry。
2. `fs_boot` 按 root block 配置选择 `ClassDevice`。
3. 调用 `RootFileSystem::mount_bdev`，链接器把调用解析到 Kconfig 所选 provider。
4. provider 校验并挂载介质，返回 canonical name 一致的 KVFS `SuperBlock`。
5. `fs_boot` 只负责把该 superblock 安装为初始 mount namespace 根。

`SeekableDisk` 对齐设备 block size 处理完整 block I/O；首尾非对齐部分使用内部缓冲。
游标跳转和读取前会先 flush 待写的 partial block，保持读后写顺序。

## 并发模型

`fs_block` 不维护全局可变 registry。`RootFileSystem` provider 在链接期确定。
`SeekableDisk` 需要 `&mut self` 执行游标和缓冲操作，共享时由上层文件系统锁保护。

## 设计决策

- Kconfig 是 root filesystem 选择的唯一配置源；Cargo feature 只完成 crate 链接。
- boot 不依赖具体 ext4/FAT crate，也不读取 `KFEAT_FS_*` 决定控制流。
- 接口放在 `fs_block` 而非 `kvfs`，避免 KVFS 反向依赖具体 block device class。
- provider name 是 KVFS type discovery 的身份，不新增 boot 侧的文件系统名称分支。

## Drop / 资源释放

`ClassDevice` 由引用计数句柄持有。`SeekableDisk` 的普通 drop 不隐式报告 flush 错误，
文件系统必须在同步或卸载路径显式调用 `flush()`。
