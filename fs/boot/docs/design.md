# fs_boot — 设计文档

## 定位

`fs_boot` 负责选择 root backing device、安装初始 root mount，并创建内核启动所需的
devfs、tmpfs、procfs、sysfs 和可选 bpffs/9P mount。它拥有初始 namespace 的布局策略，
不拥有具体 block filesystem 的实现选择。它也在用户态启动前注册本镜像内建的
filesystem type，使通用 mount 路径和 `/proc/filesystems` 读取同一组类型。

## 范围

- `src/lib.rs`：filesystem type 注册、root device 选择、初始 namespace 安装和
  boot-time mounts。

## 架构

```text
kclass block devices
        |
        v
select_root_block -- ClassDevice --> fs_block::RootFileSystem::mount_bdev
                                      |
                                      v
                                root SuperBlock
                                      |
                                      v
                         MntNamespace::init_mount_tree
                                      |
              +-----------------------+-----------------------+
              v                       v                       v
            /dev                    /proc                   /tmp ...

built-in filesystem descriptors
        |
        v
kvfs::register_filesystem
        |
        +--> POSIX mount lookup
        +--> /proc/filesystems
```

root filesystem 的 Kconfig choice 由 `kruntime` 链接成唯一的
`fs_block::RootFileSystem` provider。`fs_boot` 不依赖 ext4、FAT 或具体 ext4 backend。
这仍不同于 Linux 在启动时按 `rootfstype=` 或 registered device-backed type 走通用
mount path；在引入可供 KVFS filesystem type 消费的 pre-root block-source 边界前，
`RootFileSystem` 只承担该差异，不能作为第二个 filesystem-type registry。

本 crate 直接调用 devfs/tmpfs/procfs 等构造函数，是因为这些 mount path 和启动参数
属于初始 namespace 布局。类型发现和用户提供名称的分派由 KVFS 注册表负责；boot
只注册实现描述符，不复制 POSIX mount 的类型匹配。
devtmpfs 和当前无 namespace 分化的 sysfs 构造器发布共享 superblock，因此后续按类型
挂载能看到 boot 阶段创建的设备节点、socket 和 `/sys` 链接；构造器同时保留 internal
root mount，分别对应 Linux 的 private devtmpfs mount，以及当前没有独立 kernfs root 时
sysfs 树所需的内核 active owner。可见 mount 的卸载不会 teardown 这两棵内核维护的树。
procfs、bpffs、tmpfs
仍由各自 factory 创建新实例。

## 调用约束 / 执行上下文

所有入口运行在早期启动的普通执行路径，分配器、block class 和 KVFS 已初始化，
用户进程尚未启动。路径允许分配、加锁和设备 I/O，不可在中断上下文调用。

## 算法流程

1. 注册所选 block filesystem 和内建 nodev filesystem 描述符。
2. 枚举 block class 设备；优先匹配 `KFEAT_ROOT_BLOCK`。
3. 未匹配时按兼容策略选择最后一个设备或 secondary block。
4. 把设备交给 `fs_block::RootFileSystem::mount_bdev`。
5. 用返回的 superblock 创建 initial mount tree，并安装 init `FsStruct` root。
6. 按固定路径挂载启动期虚拟文件系统；`nosuid`、`nodev`、`noexec` 和
   `relatime` 写入各自 `Mount`，9P host share 也取得默认 `relatime`；只读策略才属于
   superblock。

## 并发模型

initial namespace 由启动 CPU 串行建立。挂载 backing device id 保存在 mutex 中，设备
移除通知可在后续执行路径删除 id 并报告 mounted filesystem 已 stale。
filesystem type 注册也只在该串行阶段执行；运行期 registry 读者不会看到半注册列表。

## 设计决策

- root backing device selection 属于 boot policy；filesystem implementation selection 不属于。
- 新增 root filesystem provider 不修改本 crate 的控制流。
- 虚拟文件系统 mount 保持显式路径布局，避免把 namespace policy 隐藏进 registry。
- filesystem type registry 只拥有类型发现和创建入口，boot 仍拥有 `/dev`、`/proc`
  等路径、挂载顺序和初始 per-mount flags。
- `/dev` 不设置 `nodev`，因为设备节点访问依赖该 mount；安全策略使用
  `nosuid,noexec`，并保留 Linux mount 路径的默认 `relatime`。

## Drop / 资源释放

initial namespace 和 root path 由 KVFS `Arc` 对象持有。设备移除通知只报告 stale
backing，不会在回调中隐式卸载正在使用的文件系统。
