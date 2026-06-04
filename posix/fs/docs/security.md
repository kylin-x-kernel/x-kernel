# posix-fs - 安全与可靠性分析

## 概述

`posix-fs` 是用户态文件系统 syscall 进入内核 VFS、fd 表和设备对象的边界层。
主要风险来自用户指针和路径字符串、文件描述符复用、跨进程 procfd 解析、
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
│  ├─ delegates object semantics to kfd/kfs/kvfs/posix-fs-owned fd objects/devfs │
│  └─ maps failures to KError/KResult                       │
└──────────────────────────────────────────────────────────┘
  │
  v
kfd resources / kfs / kvfs / device and pipe implementations
```

- 用户态参数不可信，包括空指针、无效地址、过长路径、恶意 flags、负 offset 和 fd 复用。
- `posix-fs` 信任 `posix_types` / `osvm` 对用户地址执行边界检查和错误传播。
- `posix-fs` 信任 `kfd` 在 fd 查找、复制、关闭和 descriptor flags 上保持并发安全。
- `posix-fs` 信任 `kfs` / `kvfs` / 本 crate 拥有的 fd 对象 / 设备对象执行真实读写、权限、目录和挂载语义。
- 进程当前上下文、fd 表、`FsContext` 和用户地址空间必须来自当前 syscall 执行线程。

## 外部边界 / 攻击面

| 边界 | 入口示例 | 风险来源 |
|------|----------|----------|
| 用户路径字符串 | `openat`、`linkat`、`statx`、`mount` | 空指针、非 NUL 结尾、过长字符串、路径穿越、符号链接循环 |
| 用户读写缓冲区 | `read`、`write`、`readlinkat`、`getdents64`、`statfs` | 坏地址、短缓冲区、跨页访问失败、内核信息写回格式错误 |
| 用户 iovec | `readv`、`writev`、`preadv2`、`pwritev2` | iovec 数量过大、范围溢出、读写方向错误 |
| fd 编号 | 几乎所有 fd syscall | 已关闭 fd、类型不匹配、fd 复用、`CLOEXEC`/非阻塞标志混淆 |
| 当前进程状态 | `chdir`、`openat`、`close_range`、`pipe2` | 进程资源锁、fs context 更新、umask 和当前目录语义 |
| procfd 路径 | `/proc/self/fd/<fd>`、`/proc/<pid>/fd/<fd>` | 跨进程 fd 访问、目标进程退出、fd 表并发变化 |
| VFS/设备对象 | `open`、`ioctl`、`mount`、`syncfs` | 设备特定 ioctl、终端对象、mount flags、文件系统实现差异 |
| fd-to-fd 复制 | `sendfile`、`copy_file_range`、`splice` | 源目标类型错误、offset 指针 TOCTOU、同文件重叠、阻塞语义 |

本 crate 不直接访问 MMIO、PIO、DMA、固件表或架构内联汇编。

## unsafe 代码清单

| 位置 | 调用路径 | 用途 | 不变量 / 安全理由 |
|------|----------|------|-------------------|
| `dir.rs::DirBuffer::write_entry` | `sys_getdents64 -> Directory::read_dir -> DirBuffer::write_entry` | 在临时 `Vec<u8>` 中按 `linux_dirent64` ABI 写目录项 | 写入前计算记录长度并检查 `remaining_space >= len`；`entry_ptr` 位于 `Vec` 已分配范围内；记录按 `align_of::<linux_dirent64>()` 对齐；文件名复制长度来自 `name.as_bytes().len()`；尾部写入 NUL；写完后只增加 `offset` 到已检查范围 |
| `stat.rs::statfs` | `sys_statfs` / `sys_fstatfs` | 用零初始化创建 Linux `statfs` ABI 结构 | `statfs` 是 C ABI plain data 结构；零值用于初始化 padding 和暂未显式设置字段；函数随后逐项写入返回给用户态的可见字段 |

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
   `Directory` 或 pipe endpoint，类型不匹配返回错误。
5. **范围和溢出必须显式处理**：
   `fallocate` 对负 offset/len 和 `offset + len` 溢出返回 `InvalidInput`；
   `lseek` 目录 offset 使用 `checked_add_signed`；
   定点 I/O 拒绝负 offset；
   fd-to-fd 复制写回用户 offset 时使用 `checked_add`。
6. **临时 ABI 结构不能泄露未初始化内存**：
   `statfs` 使用零初始化；
   `getdents64` 的临时 `Vec` 初始为零，目录项记录显式写入尾部 NUL。

## 权限与语义不变量

1. `O_CLOEXEC`、`FD_CLOEXEC` 和 `close_range(CLOEXEC)` 必须只影响 descriptor flags，
   不应改变底层 `FileLike` 对象。
2. `O_NONBLOCK` 和 `F_SETFL(O_NONBLOCK)` 通过 `FileLike::set_nonblocking` 作用于对象。
3. `AT_EMPTY_PATH` 是空路径按 fd 解析的必要条件。
4. `AT_SYMLINK_NOFOLLOW` / `O_NOFOLLOW` 必须影响符号链接和 procfd 解析。
5. `pipe2` 必须要么同时向用户返回两个 fd，要么清理已创建的一端。
6. `mount` 不得静默忽略会改变语义的 unsupported operation flags。
7. 创建文件和目录时应应用当前进程 `umask`。
8. `chroot` 目标必须是目录。

## 线程安全

| 资源 | 并发条件 | 风险 |
|------|----------|------|
| fd 表 | 由当前进程 resources 内部锁保护 | close/dup/lookup 竞态会导致 fd 复用或对象泄漏 |
| `FsContext` | 通过进程 fs context 锁访问 | `chdir`、`chroot` 与相对路径解析并发 |
| 目录 offset | `Directory::offset` 锁保护 | `getdents64` 和目录 `lseek` 并发更新 |
| `FileLike` 对象 | 由具体文件、pipe、设备实现负责 | 阻塞、非阻塞状态和 offset 更新语义 |
| procfd 目标 fd 表 | 读取目标进程 resources 的 fd table read lock | 目标进程退出或 fd 关闭时的生命周期 |

`posix-fs` 不定义跨资源全局锁顺序。
新增代码应缩短持锁区间，
避免在持有目录 offset 或 fd 表锁时调用可能再次访问同一资源的复杂 VFS/设备操作。

## 威胁分析

| 编号 | 威胁描述 | 边界 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|------|----------|----------|----------|
| T-01 | 用户坏指针导致内核越界读写 | 用户路径/缓冲区/iovec | 高 | 直接解引用用户地址或忽略 copy 错误 | 统一使用 `UserPtr`、`UserConstPtr`、`VmBytes`、`IoVectorBuf` 并传播 `KResult` |
| T-02 | `getdents64` 写出目录项时越过临时缓冲区 | 用户输出缓冲区 / ABI 布局 | 高 | 记录长度计算错误或未检查剩余空间 | `DirBuffer::write_entry` 先检查空间，记录长度按 ABI 对齐 |
| T-03 | `statfs` 向用户泄露未初始化内核栈数据 | 用户输出缓冲区 / ABI 布局 | 高 | C ABI 结构 padding 未初始化 | `statfs` 使用 zeroed 初始化后逐项赋值 |
| T-04 | `/proc/<pid>/fd/<fd>` 绕过普通路径解析访问不该访问的对象 | procfd 路径 | 高 | 未校验 pid/fd 语法、fd 生命周期或权限模型 | `classify_procfd_path` 拒绝无效 pid/fd；fd 表读取经 `get_process_state` 和 resources；后续权限仍依赖底层对象 |
| T-05 | `O_NOFOLLOW` 被忽略导致符号链接或 procfd 路径被跟随 | 路径 flags | 高 | open/at flags 没有传入解析层 | `resolve_open_path_source` 将 `O_NOFOLLOW` 转为 `AT_SYMLINK_NOFOLLOW`，并对 live procfd 返回 loop 错误 |
| T-06 | `pipe2` 创建失败留下半初始化 fd | fd 表 | 中 | 读端加入成功、写端加入失败 | 写端加入失败时关闭读端 |
| T-07 | `fallocate` offset/len 溢出破坏文件大小或范围操作 | scalar 参数 / VFS | 高 | 负数或 `offset + len` 溢出 | 校验非负并用 `checked_add` |
| T-08 | `copy_file_range` 同文件重叠复制造成数据破坏 | fd-to-fd 复制 | 中 | 同文件重叠检查未实现 | 已在设计文档列为限制；非零 flags 在边界被拒绝；新增完整语义前不应宣称等价 Linux |
| T-09 | 未实现的 mount/umount flags 被静默忽略 | mount flags | 高 | 用户态请求 bind、remount、detach 等语义 | 对未实现 operation flags 返回 `InvalidInput` |
| T-10 | 文件锁占位返回成功导致应用误以为互斥成立 | fd_ops | 中 | `fcntl`/`flock` 锁语义未实现 | 文档列为限制；未来实现前审计所有锁相关返回路径 |
| T-11 | 创建文件所有者固定为 root 导致 DAC 语义错误 | open/create | 高 | `current_effective_ids()` 固定 `(0, 0)` | 文档列为限制；接入 `kcred` 后应使用当前 fsuid/fsgid |
| T-12 | `faccessat2` 权限检查与真实凭据不一致 | access | 中 | 仅检查 owner 权限位，不区分 UID/GID/补充组 | 文档列为限制；未来需接入 `kcred::AccessCredentials` |
| T-13 | 设备 ioctl 参数被错误解释 | ioctl / 设备对象 | 中 | syscall 层错误处理设备私有命令 | `FIONBIO` 在本层处理，其它命令转交 `FileLike::ioctl`；常见 isatty 探测错误不刷 warning |

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
| F-04 | pipe 写端 fd 添加失败 | fd 表满或分配失败 | 读端被关闭，pipe 创建失败 | 无半创建 fd 泄漏 | 3 | `inspect_err` 清理读端 |
| F-05 | `open` 设备特殊处理失败 | `/dev/ptmx`、当前终端或 `/dev/pts` 解析失败 | open 返回错误 | 终端相关程序无法打开目标设备 | 3 | 错误传播；未知终端类型返回 `OperationNotSupported` |
| F-06 | `fallocate` 后端写零返回 0 | 底层文件系统无法前进写入 | 返回 `WriteZero` | 操作失败但文件不应继续无限循环 | 2 | `write_zeros_range` 检测 0 字节写 |
| F-07 | `sendfile`/`splice` 遇到 `WouldBlock` | 非阻塞源暂时无数据 | 已写入部分则返回部分进度，否则返回错误 | 调用方可轮询后重试 | 4 | `do_send` 保留部分写入语义 |
| F-08 | `mount` 请求不支持的文件系统 | `fs_type != "tmpfs"` | 返回 `NoSuchDevice` | 用户态 mount 失败 | 3 | 显式检查 fs_type |
| F-09 | `syncfs` 目标不是文件或目录 | fd 指向 pipe/socket/设备 | 返回 `InvalidInput` | 当前同步请求失败 | 4 | downcast 后只 flush 文件系统对象 |
| F-10 | `copy_file_range` 语义不完整 | 重叠和普通文件检查 TODO | 可能出现与 Linux 不一致的数据结果 | 相关应用复制行为异常 | 2 | 非零 flags 显式拒绝；其余限制实现前需要补充测试 |
| F-11 | `fcntl` unsupported cmd 返回成功 | 兼容占位 | 应用误判某些控制操作已生效 | 可能产生行为差异 | 2 | warning 记录；后续应按 cmd 补充错误语义 |
| F-12 | `close_range(UNSHARE)` 资源复制失败 | 下层 unshare 实现异常或未来改为可失败 | 当前 API 没有错误承载 | fd 表隔离语义不完整 | 2 | 审计 `unshare_fd_table` 语义，未来可失败时更新 syscall 返回 |

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
  这是兼容占位，不应扩展到有安全影响的新命令。
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

1. `current_effective_ids()` 固定返回 root 身份，创建者 UID/GID 语义不完整。
2. `faccessat2` 尚未完整接入真实凭据、补充组、capability 和 `AT_EACCESS` 语义。
3. `fcntl` 文件锁和 `flock` 未实现真实锁。
4. `copy_file_range` 对普通文件类型、同文件重叠和跨文件系统限制仍为 TODO。
5. `mount` 只支持 `tmpfs`，不支持 bind、remount、move、propagation 和 lazy/force unmount。
6. `preadv2` / `pwritev2` 当前只支持 `flags == 0`，
   非零 flags 返回 `Unsupported`。

## 审计清单

修改 `posix-fs` 时需验证：

- [ ] 新增用户指针访问只通过 `UserPtr`、`UserConstPtr`、`VmBytes` 或 `IoVectorBuf`。
- [ ] 新增 `unsafe` 块有 `SAFETY:` 注释，并补充到本文 unsafe 清单。
- [ ] 新增路径解析入口正确处理 `AT_EMPTY_PATH`、`AT_SYMLINK_NOFOLLOW` 和 `AT_FDCWD`。
- [ ] 新增 open/stat/namei 入口明确普通路径与 procfd 路径的差异。
- [ ] fd 表修改失败时不会留下半创建对象或错误 descriptor flags。
- [ ] 涉及 offset/len 的路径检查负数、溢出和 0 长度语义。
- [ ] 跨 fd 复制路径检查源目标类型、阻塞语义和用户 offset 写回。
- [ ] 元数据和 access 改动与 `kcred` 凭据模型保持同步。
- [ ] mount/umount 新 flags 要么完整实现，要么显式拒绝。
- [ ] 新增日志不输出文件内容或敏感用户缓冲区。
- [ ] 若修复当前已知限制，同步更新本文和 `design.md`。
