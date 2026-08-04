# posix-fs - 设计文档

## 定位

`posix-fs` 是 x-kernel 的 POSIX/Linux 文件系统 syscall 兼容 crate。
它把 `openat(2)`、`read(2)`、`write(2)`、`stat(2)`、`mkdir(2)`、
`link(2)`、`mount(2)`、`fcntl(2)`、`ioctl(2)` 等用户态入口
转换为内核内部的 fd 表、进程文件系统上下文、VFS 节点和设备对象操作。

目标读者是维护 `core/ksyscall` 文件系统分发路径、`kfd` 进程 fd 表、
`kvfs` VFS 层，以及终端和设备文件兼容路径的开发者。

## 背景

POSIX 文件系统 syscall 同时接触用户指针、路径字符串、进程当前工作目录、
文件描述符、VFS 节点、设备节点以及与 fd-backed object 的互操作路径。
`posix-fs` 把这些 Linux ABI 细节收敛到一个 crate 中，
使 syscall 分发表只负责导出入口，
而底层 VFS 和 fd 表继续保持面向内核对象的抽象。

`timerfd`、`eventfd` 这类不以 VFS 或文件系统状态为核心的 fd-backed kernel object
不再由 `posix-fs` 拥有，而是迁移到独立的 owner crate `kfd_objects`。

当前实现以兼容常用 Linux 用户态程序为目标，
优先覆盖常用路径和明确拒绝尚不支持的模式。
部分接口采用兼容占位行为，例如部分 `fcntl` 锁操作和 `flock`，
这些限制记录在本文的设计决策和已知限制中。

## 范围

涉及的源文件：

```text
posix/fs/
├── Cargo.toml
├── src/
│   ├── lib.rs          # crate 入口和 syscall re-export
│   ├── path.rs         # AT_*、dirfd、VFS magic-link 和 empty-path 解析
│   ├── open.rs         # open/openat 参数转换和 fd 安装
│   ├── io.rs           # read/write/lseek/fallocate/sendfile/splice 等 I/O
│   ├── fd_ops.rs       # close/dup/close_range/fcntl/flock
│   ├── dir.rs          # chdir/chroot/mkdir/getdents64/getcwd
│   ├── namei.rs        # link/unlink/symlink/readlink/rename
│   ├── metadata.rs     # chown/chmod/utime/utimensat
│   ├── mount.rs        # mount/umount2
│   ├── stat.rs         # stat/fstatat/statx/access/statfs
│   ├── ioctl.rs        # ioctl dispatch and FIONBIO handling
│   └── sync.rs         # sync/syncfs
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```text
core/ksyscall
    │ dispatches filesystem syscalls
    v
┌──────────────────────────────────────────────────────────┐
│ posix-fs                                                 │
│                                                          │
│  ABI parsing                                             │
│   ├─ UserConstPtr/UserPtr string and buffer access        │
│   ├─ Linux flags, modes, stat/statfs/ioctl structures     │
│   └─ errno-oriented KResult handling                      │
│                                                          │
│  Process context                                         │
│   ├─ kprocess::current_user_process()                     │
│   ├─ kprocess::current_resources()                        │
│   └─ fs_context::FsStruct { root, pwd, umask, in_exec }    │
│                                                          │
│  Internal object dispatch                                │
│   ├─ kfd::FileLike                                       │
│   ├─ kvfs::{Filename, VfsFile, MetadataUpdate, MountFlags}  │
│   ├─ kvfs::FileSystemType registry                         │
│   ├─ fs_context::FsStruct                                │
│   └─ kvfs::pipe::PipeObject interop for splice/fcntl     │
└──────────────────────────────────────────────────────────┘
```

| 子模块 | 职责 |
|--------|------|
| `path` | 统一处理绝对路径忽略 `dirfd`、`AT_FDCWD`、`AT_EMPTY_PATH`、`AT_SYMLINK_NOFOLLOW`、`O_NOFOLLOW` 和 VFS magic-link follow |
| `open` | 将 Linux open 参数交给 `kvfs::Filename`，并把 VFS 返回的打开结果加入当前进程 fd 表；不重新解析 special-file pathname |
| `io` | 处理普通、向量、定点和 fd-to-fd 数据传输 syscall |
| `fd_ops` | 维护 fd 生命周期、复制、`CLOEXEC`、非阻塞标志和部分 `fcntl` 行为 |
| `dir` | 维护当前目录、根目录、目录/节点创建和 `linux_dirent64` 输出；`mknodat` 只转换 ABI 参数，创建策略由 KVFS 执行 |
| `namei` | 处理链接、删除、符号链接和重命名等命名空间变更 |
| `metadata` | 修改所有者、权限和时间戳 |
| `mount` | 把 Linux mount flags 映射到 `kvfs::MountFlags`，按注册的 `FileSystemType` 查找实现，并处理非递归 bind 与 remount |
| `stat` | 转换 VFS metadata、access 检查和 statfs 信息 |
| `ioctl` | 处理 `FIONBIO` 并把其它命令转交 `FileLike::ioctl` |
| `sync` | 将同步请求转发到文件系统或打开对象所在文件系统 |

## 调用约束 / 执行上下文

`posix-fs` 是 syscall 层 crate，入口默认运行在当前用户进程线程上下文中。
多数函数依赖以下上下文：

- 当前线程可通过 `kprocess` 的当前线程接口获取；
- 当前进程有可访问的进程运行态、fd resources 和 `FsStruct`；
- 用户指针可通过 `posix_types` / `osvm` 访问当前地址空间；
- 调用路径允许阻塞、分配和进入 VFS、fd-backed object 或设备对象；
- 调度器、进程资源锁、VFS 和内存映射已经初始化。

该 crate 不适合作为中断上下文或早期启动阶段 API 使用。
路径解析、fd 表访问、VFS I/O、目录遍历和 fd-to-fd 复制都可能分配、
加锁、等待底层对象或返回 `WouldBlock`。

## 状态机

### fd 生命周期

```text
open/pipe/dup
    │ add_file_like / duplicate_file_like
    v
Open fd
    │ F_SETFD / close_range(CLOEXEC)
    v
Open fd with descriptor flags
    │ close / close_range
    v
Closed fd
```

`posix-fs` 不直接保存 fd 表，
而是通过 `kprocess::current_resources()` 操作当前进程资源。
### 路径解析

```text
user path pointer
    │ load_string / nullable path handling
    v
raw path string or AT_EMPTY_PATH
    │ path.rs helpers
    ├─ normal path ────────────────> kvfs::namei lookup
    ├─ empty path + AT_EMPTY_PATH ─> dirfd fd object
    └─ magic-link component ───────> kvfs::namei typed follow
```

`path.rs` 不拥有 procfd 字符串解析。
`/proc/<pid>/fd/<fd>` 由 procfs 暴露为 VFS magic-link 节点，`path.rs`
只负责把 syscall flags 转成 `LookupFlags` 与 `LookupIntent`。final 与
non-final component 的普通 symlink、magic-link、`NO_MAGIC_LINKS` 和
`O_NOFOLLOW` 规则都由 `kvfs::namei` 统一执行。

### 文件数据流

```text
user buffer / iovec
    │ VmBytes / VmBytesMut / IoVectorBuf
    v
FileLike::read/write
    │
    ├─ kvfs::VfsFile offset or read_at/write_at
    ├─ kvfs::VfsFile directory position for lseek/getdents64
    ├─ kfd_objects::PipeReadEnd / PipeWriteEnd
    └─ device-specific FileLike implementation
```

`sendfile`、`copy_file_range` 和 `splice` 共享 `SendFile`/`do_send`，
用 4 KiB 中间缓冲区在源和目标 fd 之间搬运数据。
使用用户提供 offset 指针时，读写成功后会把新的 offset 写回用户地址。

## 算法流程

### `openat`

1. 从用户指针读取路径字符串。
2. 通过 `with_fs_at` 取得包含 root、pwd 和 umask 的完整 `FsStruct` snapshot。
3. 将 Linux open flags、原始 mode 和 snapshot umask 交给 `kvfs::Filename` 的
   open 入口；umask 不在 syscall 层提前应用。
4. `kvfs` 内部构造 namei open 参数，使用 `LookupIntent::Open` 与
   `LookupFlags` 处理 `O_NOFOLLOW`、`O_PATH`、普通 symlink 和 VFS
   magic-link。
5. 对普通路径调用 `with_fs_at(dirfd, filename, ...)`：绝对路径从 root 开始且不访问
   `dirfd`，相对路径才校验并使用 `dirfd`。
6. 设备节点的特殊 open 语义由对应 VFS/device file operations 处理。
7. 根据 `O_NONBLOCK`、`O_CLOEXEC` 设置对象和 descriptor 标志。
8. 把打开的 file 加入当前进程 fd 表并返回 fd。

### `resolve_at`

1. `None` 或空路径必须配合 `AT_EMPTY_PATH`，否则返回 `NotFound`。
2. 非空路径进入 `kvfs::namei`，携带 syscall 对应的 `LookupIntent` 和
   `LookupFlags`。
3. 绝对非空路径不读取 `dirfd`；相对非空路径使用 `dirfd` 选择 base。
4. namei 在同一条路径中处理 final/non-final symlink、magic-link、
   `AT_SYMLINK_NOFOLLOW` 和 `NO_MAGIC_LINKS`。
5. 空路径配合 `AT_EMPTY_PATH` 返回 fd 对象对应的 VFS `Location` 或非 VFS
   `FileLike`。

### `mkdirat`

`mkdirat` 把原始 permission bits 和 `FsStruct` umask 交给
`Filename::mkdir_at()`。该入口在父目录 namespace lock 下完成 final lookup，再由
`Path::vfs_mkdir()` 执行 mount、父目录 DAC、mode preparation 和 filesystem callback。
如果路径已经可解析为现有对象，包括 `/`、`.`、`..` 这类没有普通 final component
的目录路径，则返回 `AlreadyExists`，对应 Linux `EEXIST`。

### `mknodat`

syscall 入口只复制 pathname，并把 32 位 ABI mode/device 截断或转换为
`Umode` 和 `DeviceId`，随后在任何 `dirfd` 访问前调用
namei 层的 `may_mknod()` 执行 Linux 类型校验，并把验证后的 `NodeType` 写回
现有 `Umode` 的类型位。`Umode::mknod_node_type()` 仅负责纯类型解码，不依赖
VFS errno。规范化后的 mode、`FsStruct` umask 和 credential snapshot 传给
`Filename::mknod_at()`：

1. syscall 边界在路径解析前按原始 `S_IFMT` 位拒绝目录、符号链接和非法类型；
2. 在父目录 exclusive namespace lock 下完成最终 lookup；
3. positive dentry 先返回 `AlreadyExists`，negative dentry 才检查 mount write、
   `MAY_WRITE | MAY_EXEC` 和 character/block device 特权；
4. 与 open-create 共用 `Path::vfs_create()` / `Path::vfs_mknod()`，按 setgid 后
   umask 的顺序准备 mode；
5. regular file 使用 exclusive create callback，FIFO/socket 清零 device，
   character/block device 保留 ABI device。

当前 capability 模型仍以 `euid == 0` 近似 Linux `CAP_MKNOD`。

### `linkat`、`symlinkat`、`unlinkat`

这些 namespace syscall 不再通过通用 `create_at()` 或完整目标 lookup 做一次锁外
final lookup。syscall 复制并校验 ABI 参数后调用对应的
`Filename::link_at()`、`Filename::symlink_at()`、`Filename::unlink_at()` 或
`Filename::rmdir_at()`：

1. Filename 解析 parent 和 final-component 类型；
2. Dentry 在 parent exclusive namespace lock 下执行唯一 final lookup；
3. positive/negative、trailing slash 和特殊 final component 的 errno 在该最终对象上决定；
4. Path 执行 mount、DAC、cross-mount、sticky 和 mountpoint 策略；
5. VfsInode 只进入 filesystem callback。

`linkat` 只接受 `AT_SYMLINK_FOLLOW | AT_EMPTY_PATH`；默认不跟随 source final symlink，
`AT_SYMLINK_FOLLOW` 才启用 follow。

### exec path source

`execve` 不再单独解析 procfd 字符串或提供专用 magic-link helper：

1. syscall 层读取用户 path 后构造 `LookupContext`。
2. 非空 path 以 `LookupIntent::Exec` 和 follow-final flags 进入
   `kvfs::namei`。
3. 普通路径、procfd magic-link、APK magic source 风格显示路径，以及后续
   `AT_EMPTY_PATH`/`fexecve` 入口都应收敛为同一套 namei/open-executable
   语义。
4. syscall 层把解析得到的 `Location` 与用户显示路径一起传给
   `process/kexec::ExecRequest::from_resolved_with_display()`，loader 不再重新按
   procfs 字符串查找目标对象。

### `getdents64`

1. 根据用户给定长度分配内核临时 `DirBuffer`。
2. 获取目录 fd，并锁住目录 offset。
3. 遍历目录项，把每项写成 ABI `linux_dirent64` 布局。
4. 缓冲区放不下一条完整记录时停止遍历。
5. 若存在目录项但用户缓冲区连一条也放不下，返回 `InvalidInput`。
6. 将临时缓冲区写回用户空间，返回实际写入字节数。

### `fallocate`

1. 校验 `offset`、`len` 非负并检查 `offset + len` 溢出。
2. 只接受当前实现支持的模式组合。
3. 普通预分配和 `UNSHARE_RANGE` 通过必要时写入最后一个零字节推进 EOF。
4. `PUNCH_HOLE` 和 `ZERO_RANGE` 用分块写零模拟。
5. `COLLAPSE_RANGE`、`INSERT_RANGE` 委托 `File` 的范围操作。

### `mount`

1. `sys_mount()` 只复制 nullable source/fs type 和 target，捕获一次 process/credential
   上下文后进入 `do_mount()`。
2. `do_mount()` 按当前 `FsStruct` 解析 target，再把 resolved `Path` 交给
   `path_mount()`；对应 Linux 的 syscall、`do_mount()` 和 `path_mount()` 分层。
3. `path_mount()` 丢弃旧 mount magic，拒绝 `MS_NOUSER`，并在操作分派前统一拒绝当前未实现
   的 move、propagation 和 recursive bits；随后从同一份 `MS_*` 参数分别生成 superblock
   flags 和 per-mount `MountFlags`。remount 未显式指定
   atime 选项时保留目标 mount 当前的 atime flags；其它 user-settable per-mount flags
   按本次请求整体替换，对应 Linux `set_mount_attributes()`。
4. 已验证的请求按 `MS_REMOUNT|MS_BIND`、普通 `MS_REMOUNT`、`MS_BIND`、普通新挂载分派；
   对应操作分别调用 namespace 的
   `reconfigure_mount()`、`remount()`、`attach_bind()` 或
   `attach_with_flags_and_devname()` 对象方法。
5. 非递归 bind 要求非空 source，解析 source 后克隆源 mount；副本继承源 mount
   flags，初次 `MS_BIND` 的普通 mount flags 不应用到副本。
6. 普通挂载像 Linux `do_new_mount()` 一样按 canonical type name 查询 KVFS
   `FileSystemType` 注册表，再调用类型描述符的 nodev 创建入口。注册表和
   `/proc/filesystems` 都只使用 canonical name，例如 `devtmpfs`。
7. nodev 创建入口接收已经提取的 `SuperBlockFlags`，在构造 superblock 时应用 VFS-wide
   策略；随后把带 per-mount flags 的 detached mount graft 到 target。

## 并发模型

`posix-fs` 本身不持有全局状态。
并发控制由下层资源对象负责：

- fd 表和 descriptor flags 由 `kfd`/进程 resources 内部锁保护；
- `FsStruct` 通过进程状态中的锁串行化当前目录、根目录、umask 和解析基准更新；
  它是 umask 的唯一 owner，`CLONE_FS` 通过共享同一个 `Arc<Mutex<FsStruct>>`
  同时共享 root、pwd 和 umask。非共享 fork 使用 `clone_for_process()` 复制状态；
  单次 `*at` 操作使用完整 `snapshot()`，不会把 umask 重置成默认值；
- 目录流 offset 由 `Directory::offset` 锁保护；
- 文件、pipe、设备、mountpoint 和 VFS 节点的内部并发由各自实现负责；
- 用户内存访问由 `posix_types` / `osvm` 在当前进程地址空间下执行。

文档使用者需要特别注意跨对象操作：
`renameat2` 会解析旧目录和新目录后交给 VFS rename；
`sendfile`/`copy_file_range`/`splice` 在一个循环里交替读写两个 fd；
设备节点的 open 语义由 VFS/device file operations 自身处理。
这些路径不在 `posix-fs` 中显式定义全局锁顺序，
因此新增代码应避免在持有 fd 表或目录 offset 锁时进入可能回调同一资源的复杂路径。

## 设计决策

### syscall 兼容层与 VFS 分离

`posix-fs` 只处理 Linux ABI、当前进程上下文和错误传播，
把实际文件语义交给 `kvfs`、`kfd`、`kfd_objects` 和设备对象。
这样可以避免在 syscall 层复制文件系统实现，
代价是 syscall 兼容行为必须清楚记录哪些由本 crate 保证、哪些由底层对象保证。

### fd-backed object 与文件系统状态分离

通过 fd 向用户态暴露对象，并不自动意味着该对象属于文件系统 owner。
`timerfd` 和 `eventfd` 位于 `kfd_objects`，其核心状态分别属于 timer runtime 与 event
counter。pipe 则位于 `kvfs::pipe`：匿名 pipe 与 pathname FIFO 共享
`pipe_inode_info` 等价状态，而 pathname FIFO session 由 inode 持有，因此属于 fs-core，
不能由 process fd-object 层拥有。`posix-fs` 对这些对象保留的职责仅限于
`splice`、`fcntl(F_*PIPE_SZ)` 等 syscall 与底层对象之间的互操作接线。

### procfd 作为 VFS magic-link

`/proc/<pid>/fd/<fd>` 由 procfs 暴露为 magic-link 节点，而不是
`posix-fs` 内部的字符串解析特例。
`posix-fs` 根据 syscall flags 决定是否 follow final component，
实际目标 fd snapshot、readlink display 和 follow 行为由 procfs/kfd/kvfs 协作完成。
这样能保留 live procfd 对象语义，同时避免 syscall 层复制 procfs 路径规则。

### mount 操作的兼容边界

`sys_mount` 的 ABI 入口、`do_mount` 的 target lookup 和 `path_mount` 的 flags
归一化/操作分派对应 Linux 同名层次；不为请求参数增加额外持久状态对象。它实现非递归
bind mount、普通 remount 和 bind remount。remount target 必须是当前 mount namespace
中已注册 mount 的根路径；普通目录返回 `InvalidInput`。普通 remount 分别传递
superblock flags 和 per-mount flags，bind remount 只更新目标 mount flags；未显式给出
atime 选项的 remount 只保留目标当前 atime mask，其它 user-settable flags 按请求重建；
普通 remount 未给出 `MS_RDONLY` 时请求把 superblock 切换为读写。这是 raw Linux
`mount(2)` 语义，用户态 `mount(8)` 是否补齐旧选项属于其自身策略。
初次 bind 继承源 mount flags，不应用同次调用的普通 mount flags；修改 bind mount flags
需要第二次 `MS_REMOUNT|MS_BIND` 调用。
普通新挂载不再依赖 devfs、procfs、bpffs 等具体 crate；`do_new_mount` 只解析 canonical
type 并使用 KVFS registry，对应 Linux `get_fs_type()` 后进入 filesystem type 的创建入口。
这使 mount 支持集合与 `/proc/filesystems` 保持同一事实源。
move、recursive bind 和 propagation flags 在任何 operation dispatch 前返回 `InvalidInput`；
因此 `MS_BIND|MS_MOVE`、`MS_REMOUNT|MS_SHARED` 和普通新挂载携带 `MS_REC` 都不会执行部分请求，
`sys_umount2` 对 force、detach、expire、nofollow 返回 `InvalidInput`。
显式拒绝比静默忽略更安全，
因为静默忽略会让用户态误以为已经获得异步卸载或 bind mount 等语义。

### 部分兼容占位

当前 `F_SETLK`、`F_SETLKW`、`F_OFD_SETLK` 和 `F_OFD_SETLKW` 返回成功，
`F_GETLK`/`F_OFD_GETLK` 写回 `F_UNLCK`，
`sys_flock` 返回成功但未实现真实锁。
这有助于部分用户态程序继续运行，
但不是完整的 POSIX/ Linux 文件锁语义。

### memfd sealing fcntl

`fd_ops` 处理 `F_GET_SEALS` 和 `F_ADD_SEALS`：

```text
sys_fcntl(F_GET_SEALS/F_ADD_SEALS)
  -> current_resources().get_file()
  -> kvfs::VfsFile::shmem_seal_bits() / add_shmem_seals()
  -> inode-scoped ShmemObjectState
```

这些命令只对 shmem-backed VFS files 有效。非 VFS file 或没有
`ShmemObjectState` 的普通文件返回错误。`F_ADD_SEALS` 只允许单调添加 seal；
已有 `F_SEAL_SEAL` 时拒绝继续添加。

### 当前凭据与 VFS 授权

每个 pathname syscall 在入口调用 `kprocess::current_cred()` 取得一个 committed
`Arc<Cred>`，并把同一快照传给 `Filename`、`Nameidata` 和 `Path`。普通 open、路径
遍历、创建、删除、链接、rename、pathname truncate 和 exec 检查使用该对象的
`fsuid/fsgid` 与补充组；VFS 不反向查询当前 task。

新 inode 由文件系统创建回调使用 `kvfs::inode_init_owner()` 初始化：UID 为 `fsuid`，
GID 通常为 `fsgid`，setgid 父目录下继承父 GID，且新子目录传播 setgid 位。

`faccessat2` 在解析路径之前选择检查身份：默认由 `Cred::for_access()` 使用 real IDs，
`AT_EACCESS` 直接使用当前 committed credential，保留其 `fsuid/fsgid`。所选对象同时
用于中间目录 search 和最终 inode 检查，补充组保持不变。

`chown/chmod/utimensat` 同样在入口取得一次凭据快照，再由 `Path` 执行 Linux 风格的
owner、group、write-permission 和显式时间授权。`chroot` 在目标目录 search 检查之后
还要求特权；当前无 capability 模型，因此用 `euid == 0` 近似 `CAP_SYS_CHROOT`。

`truncate(path)` 使用调用时凭据进行 pathname write DAC；`ftruncate(fd)` 验证打开文件
具有 write mode 后直接使用 open file authority，不因调用者随后改变 UID 而重复执行
pathname DAC。这对应 Linux `vfs_truncate()` 与 `do_ftruncate()` 的语义分工。

### 复制类 syscall 使用中间缓冲区

`do_send` 使用 4 KiB 临时缓冲区实现 fd-to-fd 复制。
实现简单并复用 `FileLike` 的 read/write 语义，
代价是没有零拷贝，也没有完整处理 `copy_file_range` 的同文件重叠和跨文件系统限制。
`copy_file_range` 当前只接受 `flags == 0`，
非零 flags 会在 syscall 边界返回 `InvalidInput`。

## Drop / 资源释放

`posix-fs` 没有自定义 `Drop` 类型。
资源释放依赖下层对象：

- `close`/`close_range` 从当前进程 fd 表移除 `Arc<dyn FileLike>`；
- fd 复制和打开路径通过 `Arc` 共享文件、目录、pipe 或设备对象；
- 临时 `Vec`、路径 `String`、`CString` 和中间 I/O 缓冲区在函数返回时释放；
- mount/unmount 的 topology 由 `kvfs::Path` 和 `Mount` 管理；`VfsMount` 最后释放时归还
  superblock active 引用，并在最后一个引用消失时执行 shutdown。

## 已知限制

1. 凭据 DAC 尚无 capability、LSM、ACL、user namespace ID 映射或 idmapped mount。
2. FAT 等不能表达 Unix UID/GID 的后端无法完整持久化创建者身份。
3. `fcntl` 文件锁和 `flock` 目前是兼容占位，不提供真实互斥。
4. `copy_file_range` 尚未检查普通文件类型、同文件重叠和跨文件系统条件。
5. `mount` 尚不支持 move、recursive bind、propagation，以及只读策略之外的文件系统专用
   reconfigure。registry 中需要 block device 的 root filesystem 会列在
   `/proc/filesystems`，但 legacy `mount(2)` 尚未实现 source block-device 解析，因此
   当前返回 `NoSuchDevice`。
6. `sendfile` 对非空 offset 保留 32 位范围限制，反映旧接口兼容约束。
7. `F_ADD_SEALS` / `F_GET_SEALS` 已接入 shmem object state；shared
   writable mmap 和 `mprotect(PROT_WRITE)` seal enforcement 由 `mm/filemap`
   执行。
