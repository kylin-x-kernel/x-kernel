# fs_boot — 设计文档

## 定位

`fs_boot` 负责选择 root backing device、安装初始 root mount，并创建内核启动所需的
devfs、tmpfs、procfs、sysfs 和可选 bpffs/9P mount。它拥有初始 namespace 的布局策略，
不拥有具体 block filesystem 的实现选择。

## 范围

- `src/lib.rs`：root device 选择、初始 namespace 安装和 boot-time mounts。

## 架构

```text
kclass block devices
        |
        v
select_root_block -- ClassDevice --> fs_block::FileSystemType::mount_bdev
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
```

root filesystem 的 Kconfig choice 由 `kruntime` 链接成唯一的
`fs_block::FileSystemType` provider。`fs_boot` 不依赖 ext4、FAT 或具体 ext4 backend。

本 crate 直接调用 devfs/tmpfs/procfs 等构造函数，是因为这些 mount path 属于初始
namespace 布局；它不是按用户提供 filesystem type 名称分派的通用 mount 路径。

## 调用约束 / 执行上下文

所有入口运行在早期启动的普通执行路径，分配器、block class 和 KVFS 已初始化，
用户进程尚未启动。路径允许分配、加锁和设备 I/O，不可在中断上下文调用。

## 算法流程

1. 枚举 block class 设备；优先匹配 `KFEAT_ROOT_BLOCK`。
2. 未匹配时按兼容策略选择最后一个设备或 secondary block。
3. 把设备交给 `fs_block::FileSystemType::mount_bdev`。
4. 用返回的 superblock 创建 initial mount tree，并安装 init `FsStruct` root。
5. 按固定路径挂载启动期虚拟文件系统。

## 并发模型

initial namespace 由启动 CPU 串行建立。挂载 backing device id 保存在 mutex 中，设备
移除通知可在后续执行路径删除 id 并报告 mounted filesystem 已 stale。

## 设计决策

- root backing device selection 属于 boot policy；filesystem implementation selection 不属于。
- 新增 root filesystem provider 不修改本 crate 的控制流。
- 虚拟文件系统 mount 保持显式路径布局，避免把 namespace policy 隐藏进 registry。

## Drop / 资源释放

initial namespace 和 root path 由 KVFS `Arc` 对象持有。设备移除通知只报告 stale
backing，不会在回调中隐式卸载正在使用的文件系统。
