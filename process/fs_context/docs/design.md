# fs_context — 设计文档

## 定位

`fs_context::FsStruct` 对应 Linux `struct fs_struct`，保存任务路径解析所需的 root、pwd、umask
和 exec transition 状态。它是进程的文件系统视图；`kprocess::ProcessRuntime` 负责持有、
共享或复制其 `Arc<Mutex<FsStruct>>`。

它与 KVFS `FsContext` 不同：`FsStruct` 是进程生命周期状态；`FsContext` 是一次文件系统
创建或重配置事务。

## 结构与所有权

```text
ProcessRuntime -- Arc<Mutex<fs_context::FsStruct>>
                              |
                              +-- root: kvfs::Path
                              +-- pwd: kvfs::Path
                              +-- umask
                              `-- in_exec

mount transaction -----------> kvfs::FsContext
```

`root` 和 `pwd` 对应 Linux 的同名字段。Rust 的 `Option<Path>` 只表达 Linux 静态
`init_fs` 在首个 mount tree 安装前零初始化这两个 `struct path` 的状态；boot 通过一次
`attach_root` 同时安装二者，不引入额外的路径环境对象。

crate 提供进程生命周期的 `FsStruct`；KVFS 的 `FsContext` 是一次 mount transaction，
两者职责不同。

## 调用约束

本 crate 用于普通任务和 early-boot VFS 上下文。访问方必须持有外层 mutex；不得在中断
上下文或持有不兼容自旋锁时进入可能释放 `Path` 的更新操作。

## 生命周期算法

- `init_fs()` 返回 init task 共享的静态对象；boot 安装首个 root 时调用 `attach_root`。
- 不带 `CLONE_FS` 的 clone 使用 `clone_for_process` 复制 root/pwd/umask，并清除 `in_exec`。
- 带 `CLONE_FS` 的线程或进程共享同一个 `Arc<Mutex<FsStruct>>`。
- chdir/chroot/mount-namespace retarget 先验证目录，再在锁内替换对应 `Path`。
- mount source lookup 在短临界区取得 `root_and_pwd()` 快照，随后释放锁再执行路径 I/O。

## 并发与设计决策

外层 `Mutex` 承担 Linux `fs_struct::seq` 和共享更新串行化的职责；`Path` 的引用计数承担
`path_get/path_put` 生命周期。crate 位于 `process/`，因为 `CLONE_FS`、fork、exec 和 exit
决定它的共享与生命周期；它依赖 KVFS 的 `Path`，但不属于任何 filesystem instance。

独立 crate 避免 `kprocess` 与 KVFS 之间形成反向依赖，同时让 namespace、exec 和 POSIX
路径代码共享同一个语义对象。
