# fs_boot — 设计文档

## 定位

`fs_boot` 拥有初始 mount namespace 的布局与 root device 选择策略，不拥有具体 block
filesystem 的 mount 算法。内建 filesystem type 在用户态启动前注册，普通 mount、boot
root 和 `/proc/filesystems` 读取同一组 descriptor。

## 架构

```text
block::block_devices -> select root policy -> /dev/<disk_name>
                                              |
bootstrap rootfs -> bootstrap devtmpfs         v
                               FsContext -> FileSystemType::get_tree
                                              |
                                              v
                                      KVFS get_tree_bdev
                                              |
                                              v
                                      real root SuperBlock
                                              |
                                              v
                                   overmount bootstrap rootfs
```

Kconfig 的 ext4/FAT 开关只决定把哪些 block filesystem 链入运行态，不预选 root 格式。
每个已链接后端通过统一 `register_init` 段注册自己的 canonical `FileSystemType` 静态对象；
`kruntime` 只执行该段，不引用或调用具体后端。`fs_boot` 不接收 root descriptor，也不定义 root
provider trait 或第二套名称/能力来源。root 候选从同一 registry 的 `REQUIRES_DEV` 类型中
取得，`get_tree` 与用户 `mount(2)` 完全相同。这对应 Linux 各文件系统 init 函数调用
`register_filesystem()`，随后 `mount_root_generic()` 读取 `list_bdev_fs_names()` 的层次。

初始 namespace 先在 KVFS structural nullfs 上挂一个名为 `rootfs` 的 ramfs，并把 init
`FsStruct` 临时指向它。随后在 `/dev` 挂共享 devtmpfs，使 root source 能按 pathname
解析。选定 root disk 后构造普通 `FsContext`，经 `get_tree_bdev` 查找或创建真实 superblock，
再把它 overmount 到 bootstrap root。最后把 init `FsStruct` 的 root/pwd 成对更新到新的
visible root。bootstrap rootfs 保留在覆盖层下，对应 Linux 初始 rootfs 的生命周期。

用户态启动前，真实 root 上再挂同一个共享 devtmpfs 以及 tmpfs、procfs、sysfs 和可选
bpffs。每个固定路径直接使用已经由 init 段注册的 canonical descriptor 构造 `FsContext`，
再调用 `MntNamespace::mount_new()`；boot 不按名称二次查表，也不直接调用具体 superblock
constructor。devtmpfs/sysfs 的 internal mount 保证可见 mount 卸载后内核维护的树仍然存活。
通用 mount helper 要求目标
目录已经存在；需要由 boot 创建的路径先通过 `ensure_directory_path()` 逐级建立，9P 的动态
`/mnt/hostshare` 与固定虚拟文件系统路径使用同一显式 policy，不把递归创建隐藏在 mount 机制中。
可写 root 缺少这些目录时 boot 会按 policy 创建；只读 root 必须像 Linux 的真实 root 一样
预先包含 `/dev`、`/tmp`、`/proc` 和 `/sys` 等必要 mountpoint，否则镜像不满足启动契约。

## 算法流程

1. 统一 init 段执行各文件系统自己拥有的注册回调，发布 canonical type。
2. 建立 bootstrap rootfs、initial namespace 和 init `FsStruct`。
3. 在 bootstrap `/dev` 挂载 devtmpfs。
4. 从非零容量的 canonical `BlockDevice` 中按 `KFEAT_ROOT_BLOCK` 选择 root；未配置时兼容
   路径按 `dev_t` 顺序选择。
5. 从 registry 遍历 `REQUIRES_DEV` 候选；先以 RW flags 完整尝试一轮，再加入
   `SB_RDONLY`/`MNT_READONLY` 完整重试一轮。每次都以 `/dev/<disk_name>` 为 source 构造
   `FsContext` 并调用 `MntNamespace::mount_new()`；与 Linux `mount_root_generic()` 一样，
   只有 `EACCES`/`EINVAL` 会继续尝试下一种格式，其它错误立即停止启动。
6. 成对更新 init root/pwd 到 overmount 后的真实 root。
7. 通过同一个 `mount_new()` 对象入口在真实 root 上安装启动期虚拟文件系统。

## 所有权与并发

initial namespace 由 boot CPU 串行建立。`block::BlockDevice` 的发布与 `dev_t` lookup 由
block core 拥有；boot 只持有选择和 mount 期间的 `Arc`。Kconfig 只控制 block filesystem
implementation 是否链接；每个已链接实现恰好把一个静态 descriptor 注册进 KVFS
registry，root 格式由 registry 探测结果决定。

`nosuid`、`nodev`、`noexec` 和 `relatime` 写入各自 `Mount`；只有 filesystem-wide 只读
状态进入 `SuperBlockFlags`。bootstrap devfs 与真实 root 上的 devfs 都不能设置 `nodev`。

## 设计决策

- root device selection 是 boot policy；source resolution 和 fill-super 是通用 VFS 机制。
- 非空 `KFEAT_ROOT_BLOCK` 是权威配置，找不到同名 disk 时停止启动，不猜测其他介质。
- `fs_boot` 不引用 ext4/FAT crate，也不维护 filesystem name switch；`kruntime` 只初始化
  统一 init 段，descriptor 注册由实现自己拥有。
- 空容量 loop disk 不参加 root fallback，但仍是正常 block-core 对象。
- 固定虚拟文件系统路径属于 initial namespace policy，保持显式。
- boot 负责递归创建其声明拥有的 mountpoint；KVFS mount helper 只挂载已解析目录。
