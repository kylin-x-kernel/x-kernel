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

root filesystem 的 Kconfig choice 链接成唯一 `fs_block::RootFileSystem` provider。provider
只返回 canonical `FileSystemType`；ext4/FAT 的 `get_tree` 与用户 `mount(2)` 完全相同。
boot 不再调用 root 专用 `mount_bdev`。

初始 namespace 先在 KVFS structural nullfs 上挂一个名为 `rootfs` 的 ramfs，并把 init
`FsStruct` 临时指向它。随后在 `/dev` 挂共享 devtmpfs，使 root source 能按 pathname
解析。选定 root disk 后构造普通 `FsContext`，经 `get_tree_bdev` 创建真实 superblock，
再把它 overmount 到 bootstrap root。最后把 init `FsStruct` 的 root/pwd 成对更新到新的
visible root。bootstrap rootfs 保留在覆盖层下，对应 Linux 初始 rootfs 的生命周期。

用户态启动前，真实 root 上再挂同一个共享 devtmpfs 以及 tmpfs、procfs、sysfs 和可选
bpffs。devtmpfs/sysfs 的 internal mount 保证可见 mount 卸载后内核维护的树仍然存活。

## 算法流程

1. 注册 root filesystem type 与内建 nodev filesystem types。
2. 建立 bootstrap rootfs、initial namespace 和 init `FsStruct`。
3. 在 bootstrap `/dev` 挂载 devtmpfs。
4. 从非零容量的 canonical `BlockDevice` 中按 `KFEAT_ROOT_BLOCK` 选择 root；未配置时兼容
   路径按 `dev_t` 顺序选择。
5. 以 `/dev/<disk_name>` 为 source 构造 `FsContext`，调用普通 `get_tree`。
6. 把真实 root overmount 到 bootstrap root，并成对更新 init root/pwd。
7. 在真实 root 上安装启动期虚拟文件系统。

## 所有权与并发

initial namespace 由 boot CPU 串行建立。`block::BlockDevice` 的发布与 `dev_t` lookup 由
block core 拥有；boot 只持有选择和 mount 期间的 `Arc`。root filesystem implementation
selection 只有链接期 provider，运行时 type identity 只有 KVFS registry。

`nosuid`、`nodev`、`noexec` 和 `relatime` 写入各自 `Mount`；只有 filesystem-wide 只读
状态进入 `SuperBlockFlags`。bootstrap devfs 与真实 root 上的 devfs 都不能设置 `nodev`。

## 设计决策

- root device selection 是 boot policy；source resolution 和 fill-super 是通用 VFS 机制。
- 非空 `KFEAT_ROOT_BLOCK` 是权威配置，找不到同名 disk 时停止启动，不猜测其他介质。
- boot 不引用 ext4/FAT crate，也不维护 filesystem name switch。
- 空容量 loop disk 不参加 root fallback，但仍是正常 block-core 对象。
- 固定虚拟文件系统路径属于 initial namespace policy，保持显式。
