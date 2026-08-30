# posix-fs - 安全与可靠性分析

## 概述

`posix-fs` 是用户态文件系统 syscall 进入内核 VFS、fd 表和设备对象的边界层。
主要风险来自用户指针和路径字符串、文件描述符复用、VFS magic-link follow、
目录项 ABI 布局、元数据权限语义、mount flags 兼容和 fd-to-fd 数据复制。

`timerfd`、`eventfd` 等不以文件系统状态为核心的 fd-backed kernel object
已迁移到独立 owner crate `kfd_objects`，不再由本 crate 承担其状态机和安全审计责任。

本 crate 含少量 Rust `unsafe` 代码，
都用于构造 Linux ABI 结构，而不是绕过 VFS 或 fd 表权限。

## 信任模型

```text
user process
  │
  │ untrusted syscall arguments:
  │   path pointers, buffers, iovecs, fd numbers, flags, modes, offsets
  v
┌──────────────────────────────────────────────────────────┐
│ posix-fs                                                 │
│                                                          │
│ trust boundary                                           │
│  ├─ validates ABI flags and scalar ranges where needed    │
│  ├─ copies user strings/buffers through posix_types/osvm  │
│  ├─ resolves fd/path against current process context      │
│  ├─ delegates object semantics to kfd/kvfs/kfd_objects           │
│  └─ maps failures to KError/KResult                       │
└──────────────────────────────────────────────────────────┘
  │
  v
kfd resources / kvfs / device and pipe implementations
```

- 用户态参数不可信，包括空指针、无效地址、过长路径、恶意 flags、负 offset 和 fd 复用。
- `posix-fs` 信任 `posix_types` / `osvm` 对用户地址执行边界检查和错误传播。
- `posix-fs` 信任 `kfd` 在 fd 查找、复制、关闭和 descriptor flags 上保持并发安全。
- `posix-fs` 信任 `kvfs` / `kfd_objects` / 设备对象执行真实读写、权限、目录和挂载语义。
- 进程当前上下文、fd 表、`FsStruct` 和用户地址空间必须来自当前 syscall 执行线程。

## 外部边界 / 攻击面

| 边界 | 入口示例 | 风险来源 |
|------|----------|----------|
| 用户路径字符串 | `openat`、`linkat`、`statx`、`mount` | 空指针、非 NUL 结尾、过长字符串、路径穿越、符号链接循环 |
| mount data 字节页 | `mount(..., data)` | 坏指针、跨不可读页、二进制内容，以及由具体 filesystem 解释的恶意或未知 option |
| 用户读写缓冲区 | `read`、`write`、`readlinkat`、`getdents64`、`statfs` | 坏地址、短缓冲区、跨页访问失败、内核信息写回格式错误 |
| xattr 名称和值 | `set/get/list/remove*xattr` | 非 NUL 结尾、超长 name/value/list、非 UTF-8 suffix、短输出缓冲区、namespace 越权 |
| 用户 iovec | `readv`、`writev`、`preadv2`、`pwritev2` | iovec 数量过大、范围溢出、读写方向错误 |
| fd 编号 | 几乎所有 fd syscall | 已关闭 fd、类型不匹配、fd 复用、`CLOEXEC`/非阻塞标志混淆 |
| 当前进程状态 | `chdir`、`openat`、`close_range` | 进程资源锁、fs_struct 更新、umask 和当前目录语义 |
| VFS magic link | `/proc/self/fd/<fd>`、`/proc/<pid>/fd/<fd>` | 跨进程 fd 访问、目标进程退出、fd 表并发变化、no-follow 策略 |
| VFS/设备对象 | `open`、`ioctl`、`mount`、`syncfs` | 设备特定 ioctl、终端对象、mount flags、文件系统实现差异 |
| FIEMAP 可变长输出 | `ioctl(FS_IOC_FIEMAP)` | 用户声明的 extent 数量、header 后数组地址、后端返回的块映射 |
| fd-to-fd 复制 | `sendfile`、`copy_file_range`、`splice` | 源目标类型错误、offset 指针 TOCTOU、同文件重叠、阻塞语义 |

本 crate 不直接访问 MMIO、PIO、DMA、固件表或架构内联汇编。

## unsafe 代码清单

| 位置 | 调用路径 | 用途 | 不变量 / 安全理由 |
|------|----------|------|-------------------|
| `dir.rs::DirBuffer::write_entry` | `sys_getdents64 -> Directory::read_dir -> DirBuffer::write_entry` | 在临时 `Vec<u8>` 中按 `linux_dirent64` ABI 写目录项 | 写入前计算记录长度并检查 `remaining_space >= len`；`entry_ptr` 位于 `Vec` 已分配范围内；记录按 `align_of::<linux_dirent64>()` 对齐；文件名复制长度来自 `name.as_bytes().len()`；尾部写入 NUL；写完后只增加 `offset` 到已检查范围 |
| `stat.rs::statfs` | `sys_statfs` / `sys_fstatfs` | 用零初始化创建 Linux `statfs` ABI 结构 | `statfs` 是 C ABI plain data 结构；零值用于初始化 padding 和暂未显式设置字段；函数随后逐项写入返回给用户态的可见字段 |
| `ioctl.rs::FiemapHeader` 的 `UserRead`/`UserWrite` 实现 | `sys_ioctl(FS_IOC_FIEMAP)` | 复制 Linux FIEMAP header | `repr(C)` 结构仅含整数，末尾显式 `reserved` 覆盖 ABI 对齐且任意 bit pattern 有效；每个字节都属于已初始化字段 |
| `ioctl.rs::FiemapExtent` 的 `UserWrite` 实现 | FIEMAP writer 输出 | 复制 Linux FIEMAP extent | `repr(C)` 结构所有 ABI reserved 区域都由显式整数数组覆盖并在每次输出时清零，不存在未初始化 padding |

新增 `unsafe` 块必须同时满足：

- 代码前有 `SAFETY:` 注释；
- 本清单记录调用路径、被保护的不变量和失败后果；
- 不把用户指针裸传给底层对象；
- 不在绕过 VFS/fd 表权限检查的情况下构造对象引用。

## 内存安全不变量

1. **用户内存只通过封装类型访问**：
   syscall 入口使用 `UserConstPtr`、`UserPtr`、`VmBytes`、`VmBytesMut`
   和 `IoVectorBuf` 读写用户地址，不直接解引用用户指针。
2. **用户输出先构造到内核临时缓冲区**：
   `getdents64` 先写入 `DirBuffer`，再通过 `write_vm_slice` 拷贝给用户态。
3. **fd 对象通过 `Arc<dyn FileLike>` 持有**：
   fd 查找得到的对象在当前操作期间由引用计数保持存活。
4. **类型相关操作先 downcast 校验**：
   `ftruncate`、`fstatfs`、定点 I/O 和目录 seek 等路径先确认对象是 `File`、
   `Directory`；pipe 专用操作确认 `VfsFile::private_data` 中存在 `PipeObject`，
   类型不匹配返回错误。
5. **范围和溢出必须显式处理**：
   `fallocate` 对负 offset/len 和 `offset + len` 溢出返回 `InvalidInput`；
   `lseek` 目录 offset 使用 `checked_add_signed`；
   定点 I/O 拒绝负 offset；
   fd-to-fd 复制写回用户 offset 时使用 `checked_add`。
6. **临时 ABI 结构不能泄露未初始化内存**：
   `statfs` 使用零初始化；
   `getdents64` 的临时 `Vec` 初始为零，目录项记录显式写入尾部 NUL。
7. **FIEMAP 可变长输出必须使用 checked arithmetic**：
   `fm_extent_count` 先受 Linux UAPI 总字节上限约束，header 地址、数组字节数和每项
   offset 均用 `checked_add`/`checked_mul`；每个输出 extent 的 reserved 字段显式清零。
8. **mount data 只能以有界内核字节页向下传递**：
   syscall 入口复制一个 4 KiB base-page 大小的 opaque buffer 并强制清零末字节，不施加 UTF-8
   或提前 NUL 约束；`FsContext` 不保存用户指针，filesystem 若要在 `get_tree()` 返回后保留
   选项，必须只复制已解析状态。

## 权限与语义不变量

1. `O_CLOEXEC`、`FD_CLOEXEC` 和 `close_range(CLOEXEC)` 必须只影响 descriptor flags，
   不应改变底层 `FileLike` 对象。
2. `O_NONBLOCK` 和 `F_SETFL(O_NONBLOCK)` 必须修改 open file description 的
   `VfsFile::f_flags`，`nonblocking()` 只能从该字段派生。
3. `AT_EMPTY_PATH` 是空路径按 fd 解析的必要条件。
4. 非空绝对 `*at` pathname 必须忽略 `dirfd`；只有相对 pathname 才允许
   `dirfd` 查找失败影响结果。
5. `AT_SYMLINK_NOFOLLOW` / `O_NOFOLLOW` 必须影响符号链接和 VFS magic-link follow。
6. `mount` 不得静默忽略会改变语义的 unsupported operation flags。
7. 普通新挂载只能精确查找已注册的 filesystem type；syscall 层不得维护与
   `/proc/filesystems` 分离的构造分支。
8. 创建文件和目录时应应用 `FsStruct` 中的当前 umask；写入 umask 必须截断为
   `0777`，`CLONE_FS` 必须与 root/pwd 一起共享它。
9. `chroot` 目标必须是目录，且当前 capability 模型下调用者必须满足 `euid == 0`。
10. `chown/chmod/utimensat` 必须把同一个 credential snapshot 传给 VFS metadata
   授权，不能直接调用后端 `setattr`。
11. `F_ADD_SEALS` 只能单调添加 memfd seals，且必须尊重 `F_SEAL_SEAL`。
12. Xattr 输入 name/value 必须先复制到有上限的内核缓冲区；list name sink 的累计长度不得
    超过 `XATTR_LIST_MAX`。`size == 0` 查询不得访问输出指针或物化名称序列，非零短缓冲区
    必须返回 `ERANGE`，raw set flags 不得进入 KVFS。

## 线程安全

| 资源 | 并发条件 | 风险 |
|------|----------|------|
| fd 表 | 由当前进程 resources 内部锁保护 | close/dup/lookup 竞态会导致 fd 复用或对象泄漏 |
| `FsStruct` | 通过进程 fs_struct 锁访问 | `chdir`、`chroot`、umask 与相对路径解析并发；`CLONE_FS` 共享同一对象 |
| 目录 offset | `Directory::offset` 锁保护 | `getdents64` 和目录 `lseek` 并发更新 |
| `FileLike` 对象 | 由具体文件、pipe、设备实现负责 | 阻塞、非阻塞状态和 offset 更新语义 |
| procfs magic-link target | procfs/kfd snapshot 持有目标 `FileLike` 引用 | 目标进程退出或 fd 关闭时的生命周期 |

`posix-fs` 不定义跨资源全局锁顺序。
新增代码应缩短持锁区间，
避免在持有目录 offset 或 fd 表锁时调用可能再次访问同一资源的复杂 VFS/设备操作。

## 威胁分析

| 编号 | 威胁描述 | 边界 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|------|----------|----------|----------|
| T-01 | 用户坏指针导致内核越界读写 | 用户路径/缓冲区/iovec | 高 | 直接解引用用户地址或忽略 copy 错误 | 统一使用 `UserPtr`、`UserConstPtr`、`VmBytes`、`IoVectorBuf` 并传播 `KResult` |
| T-02 | `getdents64` 写出目录项时越过临时缓冲区 | 用户输出缓冲区 / ABI 布局 | 高 | 记录长度计算错误或未检查剩余空间 | `DirBuffer::write_entry` 先检查空间，记录长度按 ABI 对齐 |
| T-03 | `statfs` 向用户泄露未初始化内核栈数据 | 用户输出缓冲区 / ABI 布局 | 高 | C ABI 结构 padding 未初始化 | `statfs` 使用 zeroed 初始化后逐项赋值 |
| T-04 | `/proc/<pid>/fd/<fd>` magic-link 绕过普通路径解析访问不该访问的对象 | VFS magic-link | 高 | procfs follow 未保持 fd 生命周期或缺少权限模型 | procfs fd entry 通过 fd snapshot 持有目标对象；非 VFS 目标 follow 返回不支持；跨进程权限策略仍需收紧 |
| T-05 | `O_NOFOLLOW` 被忽略导致符号链接或 magic-link 被跟随 | 路径 flags | 高 | open/at flags 没有传入解析层 | syscall 层统一把 flags 转成 `LookupFlags`，`kvfs::namei` 按 `LookupIntent` 处理 final/non-final symlink 与 magic-link |
| T-06 | `fallocate` offset/len 溢出破坏文件大小或范围操作 | scalar 参数 / VFS | 高 | 负数或 `offset + len` 溢出 | 校验非负并用 `checked_add` |
| T-07 | `copy_file_range` 同文件重叠复制造成数据破坏 | fd-to-fd 复制 | 中 | 同文件重叠检查未实现 | 已在设计文档列为限制；非零 flags 在边界被拒绝；当前不宣称等价 Linux |
| T-08 | mount flags 被错误分层、类型分派漂移或 unsupported 语义被静默忽略 | mount flags / filesystem type | 高 | 用户态把 move、shared/slave/unbindable propagation 或 recursive-bind bit 与 bind/remount/new mount 组合，或 syscall/procfs 各自维护支持集合 | `path_mount` 在任何 operation dispatch 前统一拒绝未实现位，再独立生成 superblock/per-mount flags；`MS_PRIVATE[|MS_REC]` 仅对 mount root 成功，因为 KVFS mount tree 天生不传播；初次 bind 按 Linux 继承源 flags 并忽略普通请求位，bind remount 才替换 flags；普通 remount 仅默认保留 atime mask；新挂载和 `/proc/filesystems` 共用 KVFS registry |
| T-09 | 文件锁占位返回成功导致应用误以为互斥成立 | fd_ops | 中 | `fcntl`/`flock` 锁语义未实现 | 文档列为限制；锁相关返回路径必须按当前占位语义审计 |
| T-10 | 创建文件使用错误所有者导致 DAC 语义错误 | open/create | 高 | 固定 root owner，或忽略 setgid 父目录 | syscall 传递当前 `Cred`，后端用 `inode_init_owner()` 初始化 fsuid/fsgid 与继承组 |
| T-11 | `faccessat2` 权限检查使用错误身份 | access | 高 | 默认错误使用 filesystem IDs，或 `AT_EACCESS` 强行覆盖显式 fs ID | 默认构造 real-ID credential；`AT_EACCESS` 直接使用当前 credential 的 `fsuid/fsgid`，并用于完整遍历与最终检查 |
| T-12 | 设备 ioctl 参数被错误解释 | ioctl / 设备对象 | 中 | syscall 层错误处理设备私有命令 | `FIONBIO` 在本层处理，其它命令转交 `FileLike::ioctl`；常见 isatty 探测错误不刷 warning |
| T-13 | memfd seals 被非 shmem fd 或普通文件伪造 | fcntl / shmem | 中 | `F_ADD_SEALS` / `F_GET_SEALS` 没有校验 fd object 类型和 inode state | fcntl 先取得 `kvfs::VfsFile`，再由 shmem inode state 判断是否存在 |
| T-14 | 同一 pathname 操作混用多个凭据快照 | syscall/VFS | 高 | 路径组件逐次查询 current task，期间 credential 被提交 | syscall 入口只取得一次 `Arc<Cred>` 并沿完整操作传递 |
| T-15 | `ftruncate` 在 UID 改变后错误重做 pathname DAC | fd/VFS | 中 | fd 操作调用 `Path::truncate(cred)` | `VfsFile::truncate()` 只验证 open write mode，再使用 opened-file authority |
| T-16 | 非 owner 绕过 VFS 直接修改 mode、owner 或时间 | metadata | 高 | syscall 直接调用后端 `setattr` | syscall 只做 ABI 转换，`Path::chown/chmod/set_times` 统一执行 owner、group 和 write authorization |
| T-17 | 普通用户改变进程 root | chroot | 高 | 只验证目标目录可搜索 | 完成目录 DAC 后额外要求 `euid == 0`，近似 Linux `CAP_SYS_CHROOT` |
| T-18 | 普通用户通过 `mknodat` 创建设备节点 | namespace / device | 高 | 仅按 mode 创建 special inode | character/block device 要求 privileged credential；其它节点类型仍应用 umask 与 VFS 目录授权 |
| T-19 | 绝对 `*at` 路径错误访问无效 `dirfd` | pathname / fd | 中 | 在判断 pathname 是否绝对之前解析 `dirfd` | 所有非空 pathname 入口复用 `with_fs_at`；绝对路径强制选择进程 cwd snapshot 并由 KVFS 从 root 开始 |
| T-20 | `mknodat` 锁外授权与最终名称状态竞争 | namespace / device | 高 | syscall 先做特权/DAC，再由 VFS 查找或创建 | syscall 只转换 ABI；KVFS 在父目录 exclusive lock 内按 positive-first 顺序执行最终 lookup、授权、mode 准备和 callback |
| T-21 | 非法 mknod 类型被无效 dirfd 错误遮蔽 | syscall / fd | 中 | 进入 `with_fs_at` 后才执行 `may_mknod` | 复制 pathname 后立即调用 namei 层 `may_mknod()`，验证成功后才允许解析相对路径的 `dirfd` |
| T-22 | umask 出现两份 owner 或被高位污染 | process / fs context | 高 | process runtime 与 FsStruct 各保存一份，或 `sys_umask` 未截断 | `FsStruct` 是唯一 owner，`replace_umask` 统一执行 `mask & 0777`；fork/clone 与 pathname snapshot 都复制或共享该对象 |
| T-23 | namespace syscall 对 final name 做两次 lookup 并操作同名替代对象 | pathname / namespace | 高 | syscall 先解析完整目标或锁外确认目标不存在，再由 Path 按名称重新查找 | link/symlink/unlink/rmdir 调用专用 `Filename::*_at`；Dentry 在父目录 exclusive lock 下执行唯一 final lookup |
| T-24 | `linkat` 默认错误跟随 source symlink 或静默接受未知 flags | pathname flags | 中 | 通用 resolve helper 默认 follow，或 syscall 只记录 warning | syscall 只接受 `AT_SYMLINK_FOLLOW | AT_EMPTY_PATH`；默认显式使用 no-follow，未知位返回 `EINVAL` |
| T-25 | FIFO open 根据第一次错误重新解析 pathname | pathname/open | 高 | 两次 lookup 之间目标被 rename 或 symlink replacement | syscall open 只调用一次 `Filename::open_with_flags_at`；FIFO dispatch 在 KVFS 已授权的 resolved inode 上完成 |
| T-26 | FIEMAP 数量或地址计算溢出导致越界写或内核数据泄漏 | ioctl / 用户输出 | 高 | 信任 `fm_extent_count`、用未检查指针运算或复制未初始化 reserved 字段 | 限制最大数量，所有地址运算使用 checked arithmetic，输出结构显式初始化全部字段；用户地址只通过 `UserPtr` 写入 |
| T-27 | `listxattr` 的名称聚合溢出或 size query 产生无界中间分配 | xattr / 用户输出 | 中 | 先收集属性和值，再构造多个完整名称副本，或累计长度未检查 | KVFS borrowed-name sink 直接写入单个 `XattrListWriter`；逐项 checked_add 并限制 `XATTR_LIST_MAX`，`size == 0` 只计数 |
| T-28 | mount data 导致无界用户内存读取、错误拒绝 binary data 或把瞬时用户指针带入文件系统 | mount data / filesystem type | 高 | 通用层把 `void *` 当路径字符串解析，或直接向后端保留用户指针 | syscall 边界复制一个零填充的 4 KiB opaque byte page，并强制清零末字节；`FsContext` 只借用内核 slice，后端只保存解析后的 mount state |

影响等级定义：

- 高：可能导致内存破坏、内核信息泄露、权限提升或越权访问。
- 中：可能导致数据损坏、错误兼容语义、服务不可用或应用同步失效。
- 低：主要导致错误码不精确、性能退化或日志噪声。

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 路径读取失败 | 用户指针无效或字符串不可访问 | syscall 返回地址错误相关 `KError` | 当前操作失败 | 3 | `load_string` 失败直接传播 |
| F-02 | fd 类型不匹配 | 对目录执行文件写、对非 pipe 执行 pipe 操作 | 返回 `BadFileDescriptor`、`InvalidInput` 或底层错误 | 应用收到 Linux errno 等价错误 | 3 | 使用 `get_file_like_as` / downcast 校验 |
| F-03 | 用户输出缓冲区太小 | `getdents64` 一条记录也放不下，`getcwd` size 不足 | 返回 `InvalidInput` 或 `OutOfRange` | 调用方可扩大缓冲区重试 | 4 | 写入前计算长度 |
| F-04 | 设备 open 失败 | 设备 file operations 拒绝 open 或初始化失败 | open 返回错误 | 终端相关程序无法打开目标设备 | 3 | 错误传播；设备语义由对应设备实现维护 |
| F-05 | `fallocate` 后端写零返回 0 | 底层文件系统无法前进写入 | 返回 `WriteZero` | 操作失败但文件不应继续无限循环 | 2 | `write_zeros_range` 检测 0 字节写 |
| F-06 | `sendfile`/`splice` 遇到 `WouldBlock` | 非阻塞源暂时无数据 | 已写入部分则返回部分进度，否则返回错误 | 调用方可轮询后重试 | 4 | `do_send` 保留部分写入语义 |
| F-07 | `mount` 请求未知文件系统类型或无效 backing source | 名称未注册，或 source 不是可用 block-special path | 返回 `NoSuchDevice`、`ENOTBLK`、`ENXIO` 等对应错误 | 用户态 mount 失败 | 3 | 按 KVFS registry 精确查找；nodev/device-backed 类型统一经 `FsContext`，设备 source 由 `get_tree_bdev` 校验 |
| F-08 | `syncfs` 目标不是文件或目录 | fd 指向 pipe/socket/设备 | 返回 `InvalidInput` | 当前同步请求失败 | 4 | downcast 后只 flush 文件系统对象 |
| F-09 | `copy_file_range` 语义不完整 | 重叠和普通文件检查 TODO | 可能出现与 Linux 不一致的数据结果 | 相关应用复制行为异常 | 2 | 非零 flags 显式拒绝；其余限制实现前需要补充测试 |
| F-10 | `fcntl` unsupported cmd 返回成功 | 兼容占位 | 应用误判某些控制操作已生效 | 可能产生行为差异 | 2 | warning 记录；有安全影响的命令应显式实现或拒绝 |
| F-11 | `close_range(UNSHARE)` 无法取得 files owner | 进程退出已脱离 fd table owner | `unshare_fd_table` 返回 `NoSuchProcess` | 当前 syscall 失败，已脱离的 fd table 不会被重新安装 | 3 | `sys_close_range` 通过 `?` 将资源层错误传播为用户态 syscall 错误 |
| F-12 | FIEMAP 输出容量不足或中途遇到坏用户页 | 调用者提供较小数组或不可写地址 | 返回已统计数量或 `BadAddress` | 当前查询失败，文件系统状态不变 | 3 | `FiemapExtentInfo` 达到容量后正常停止；writer 每项通过 `UserPtr` 写入并传播 copy fault |
| F-13 | mount data 复制失败 | 首字节不可读，或整页读取失败后逐字节恢复也无法读取首字节 | `mount(2)` 在创建 superblock 前返回 `BadAddress` | 当前挂载不发生，不留下部分 topology | 3 | 先尝试整页复制，失败后恢复可读前缀并零填充尾部；只有零字节可读时传播错误 |

严重度定义：

- 1：致命，可能导致内核崩溃、内存破坏或不可恢复的数据损坏。
- 2：严重，功能不可用、语义错误或需要重启/人工恢复。
- 3：一般，单次 syscall 失败或应用可降级。
- 4：轻微，调用方可重试或影响有限。

## 故障管理

- 所有 syscall 入口返回 `KResult<isize>`，
  错误由 `core/ksyscall` 上层映射为 Linux errno。
- 坏 fd、坏路径、非法 flags 和不支持的模式优先返回显式 `KError`。
- `WouldBlock`、`WriteZero`、`BrokenPipe` 等 I/O 错误由底层对象传播。
- 对常见探测型 ioctl 失败，例如非终端 fd 上的 `TCGETS` / `TIOCGWINSZ`，
  不记录 warning，避免日志噪声。
- 对不支持的 `fcntl` 参数当前记录 warning 但返回成功，
  这是兼容占位，不应扩展到有安全影响的新命令。`F_ADD_SEALS` 和
  `F_GET_SEALS` 已显式接入 shmem object state。
- 本 crate 没有统一重试机制；
  用户态或上层 syscall 调度负责根据 errno 决定重试。

## 隐私分析

`posix-fs` 处理路径名、目录项名称、文件内容缓冲区、fd 编号、挂载点和设备控制参数。
模块自身不持久化这些数据，
但 debug 日志会记录部分路径、fd、flags、mode 和 ioctl cmd。
新增日志应避免输出文件内容、用户缓冲区数据或跨进程敏感路径关系。

`read`、`write`、`sendfile`、`copy_file_range` 和 `splice`
会在内核中搬运用户文件内容，
但内容不会被本 crate 解析或保存在全局状态中。

## 已知限制

1. 凭据 DAC 尚无 capability、LSM、ACL、user namespace ID 映射和 idmapped mount。
2. FAT 等不能表达 Unix owner 的后端无法完整保存创建者 UID/GID。
3. `fcntl` 文件锁和 `flock` 未实现真实锁。
4. `copy_file_range` 对普通文件类型、同文件重叠和跨文件系统限制仍为 TODO。
5. `mount` 支持已注册 nodev filesystem、已接入 `get_tree_bdev()` 的 block-backed type、
   非递归 bind、普通只读 remount、bind remount，以及普通新挂载的一页 opaque mount data
   转交；支持对 mount root 幂等执行 `MS_PRIVATE[|MS_REC]`，不支持 move、recursive bind、
   shared/slave/unbindable propagation、文件系统专用 reconfigure 和 lazy/force
   unmount。通用层支持 binary data 转交不等于具体 filesystem 已实现相应格式或选项。
6. `preadv2` / `pwritev2` 当前只支持 `flags == 0`，
   非零 flags 返回 `Unsupported`。
7. memfd `F_ADD_SEALS` / `F_GET_SEALS` 已接入；shared writable mmap 和
   `mprotect(PROT_WRITE)` seal enforcement 由 `mm/filemap` 执行。
8. POSIX ACL 与 LSM 尚未实现；`security.*`/`system.*` 的最终安全策略受这一限制。
   KExt4 当前只对外暴露普通 `user.*`、`trusted.*` 和 `security.*` xattr，KVFS 在完整
   LSM/capability hook 接入前要求 privileged credential 才能 set/remove `security.*`。

## 审计清单

修改 `posix-fs` 时需验证：

- [ ] 新增用户指针访问只通过 `UserPtr`、`UserConstPtr`、`VmBytes` 或 `IoVectorBuf`。
- [ ] 新增 `unsafe` 块有 `SAFETY:` 注释，并补充到本文 unsafe 清单。
- [ ] 新增路径解析入口正确处理绝对路径忽略 `dirfd`、`AT_EMPTY_PATH`、
  `AT_SYMLINK_NOFOLLOW` 和 `AT_FDCWD`。
- [ ] 新增 open/stat/namei 入口明确普通路径与 procfd 路径的差异。
- [ ] fd 表修改失败时不会留下半创建对象或错误 descriptor flags。
- [ ] 涉及 offset/len 的路径检查负数、溢出和 0 长度语义。
- [ ] FIEMAP flag 在 ABI 边界显式支持或按 `EBADR` 规则拒绝，输出 reserved 字段全部初始化。
- [ ] 跨 fd 复制路径检查源目标类型、阻塞语义和用户 offset 写回。
- [ ] 元数据和 access 改动与 `kcred` 凭据模型保持同步。
- [ ] 每个多步 pathname 操作只取得一次 credential snapshot，并传递给完整解析过程。
- [ ] pathname 与 descriptor 操作没有混淆调用时 DAC 和 open-file authority。
- [ ] mount/umount 新 flags 要么完整实现，要么显式拒绝。
- [ ] mount data 是否在 syscall 边界按一页 opaque bytes 有界复制，且 filesystem 只保存解析后的状态？
- [ ] 新 filesystem type 通过 KVFS registry 接入，不在 syscall 层增加具体 crate 依赖或
  与 `/proc/filesystems` 分离的名称表。
- [ ] 新增日志不输出文件内容或敏感用户缓冲区。
- [ ] xattr list 是否逐项检查累计长度、只使用一个输出缓冲区，并在 `size == 0` 时只计数？
- [ ] 若修复当前已知限制，同步更新本文和 `design.md`。
