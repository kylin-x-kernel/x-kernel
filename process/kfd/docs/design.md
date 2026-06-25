# kfd - 设计文档

## 定位

`kfd` 是 x-kernel 的进程文件描述符运行时 crate。
它提供进程本地 `FdTable`、单个 `FileDescriptor` 条目、
`FdSnapshot` 稳定视图、`FileLike` trait 抽象以及内核态 `Kstat` 到 Linux ABI `stat/statx`
的转换。

目标读者是维护 POSIX 文件 syscall、进程资源复制、socket/pipe/timerfd/eventfd
等 descriptor-backed 对象的开发者。

## 背景

POSIX 进程通过小整数 fd 引用各种内核对象。
这些对象来自 VFS 文件、目录、socket、pipe、timerfd、eventfd、pidfd、epoll 等不同子系统，
但 syscall 层需要统一执行 read/write/stat/ioctl/mmap/poll、dup、close 和 exec 时关闭。
`kfd` 把 fd 槽位管理与 file-like 操作抽象集中到一个 crate，
上层 `kresources` 负责把表挂到进程状态并提供锁。

## 范围

涉及的源文件：

```text
process/kfd/
├── src/
│   ├── lib.rs                  # crate 入口和公开 re-export
│   ├── fd_table.rs             # FdTable 槽位管理、dup、close、cloexec
│   ├── file_descriptor.rs      # FileDescriptor / FdSnapshot: Arc<dyn FileLike> + flags
│   ├── file_like.rs            # FileLike trait 与 IoSrc/IoDst type aliases
│   └── stat.rs                 # Kstat 与 Linux stat/statx ABI 转换
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```text
posix/fs, posix/net, posix/mm, io-mpx
        │
        │ current process resources
        v
┌─────────────────────────────────────────────┐
│ kresources                                  │
│  Arc<RwLock<FdTable>>                       │
└──────────────────┬──────────────────────────┘
                   │ read/write lock
                   v
┌─────────────────────────────────────────────┐
│ kfd::FdTable                                │
│  FlattenObjects<FileDescriptor, FILE_LIMIT> │
└──────────────────┬──────────────────────────┘
                   │ fd -> FileDescriptor
                   v
┌─────────────────────────────────────────────┐
│ kfd::FileDescriptor                         │
│  Arc<dyn FileLike>                          │
│  cloexec: bool                              │
└──────────────────┬──────────────────────────┘
                   │ dynamic dispatch
                   v
VFS file / directory / socket / pipe / epoll / timerfd / eventfd / pidfd
```

| 组件 | 职责 |
|------|------|
| `FdTable` | 分配、查找、复制、关闭 fd 槽位；维护 close-on-exec 标志 |
| `FileDescriptor` | 保存一个 file-like 对象的共享引用和 descriptor flags |
| `FdSnapshot` | 在 fd 表锁内复制 fd 号、`Arc<dyn FileLike>` 和 descriptor/object flags，供 procfs magic link、exec 等路径无锁使用 |
| `FileLike` | 统一 read/write/stat/path/ioctl/mmap/open flags/nonblocking 接口 |
| `IoSrc` / `IoDst` | syscall I/O 路径使用的 buffer trait object 类型 |
| `Kstat` | 内核元数据结构，负责转换为 Linux `stat` / `statx` ABI |


## 状态机

### fd 槽位生命周期

```text
Free
  │ add / add_at / add_file_like
  v
Open(cloexec = false/true)
  │ set_cloexec
  v
Open(updated cloexec)
  │ duplicate_to
  ├──────────────► Open at new fd (Arc cloned)
  │
  │ close / close_range / close_cloexec_files / close_all_if_unshared
  v
Free
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Free | Open | `add`、`add_at` 或 `add_file_like` 插入 `FileDescriptor` |
| Open | Open | `set_cloexec` 只修改 descriptor flag |
| Open | Open + Open | `duplicate_to` 克隆 `FileDescriptor`，共享同一 `Arc<dyn FileLike>` |
| Open | Free | `remove`、`close_file_like`、`close_range` 或 exec 清理 |
| Any | Rejected | fd 不存在、目标槽位越界或超过 `max_nofile` |

### close-on-exec 流程

```text
Open(cloexec = true)
   │ close_cloexec_files()
   v
Free

Open(cloexec = false)
   │ close_cloexec_files()
   v
Open
```

`close_cloexec_files` 先收集要关闭的 fd 列表，
再逐个删除。
这样可以避免边遍历 `FlattenObjects` 边修改同一结构。

### fd table 共享和关闭

```text
Arc strong_count > 1
   │ close_all_if_unshared
   v
unchanged

Arc strong_count == 1
   │ close_all_if_unshared
   v
all descriptors removed, lock dropped before FileLike drops
```

该路径用于进程资源释放。
当 fd table 仍被其他进程或线程共享时，
不会关闭所有 fd。

## 算法流程

### 添加 file-like 对象

1. `kresources` 或资源拥有者持有 `FdTable` 写锁。
2. `add_file_like` 比较当前 `count()` 与调用方传入的 `max_nofile`。
3. 构造 `FileDescriptor::new(file_like, cloexec)`。
4. `FlattenObjects::add` 选择第一个空槽位。
5. 成功时返回 fd；超过软限制或无空槽时返回 `TooManyOpenFiles`。

### 查找 typed file-like 对象

1. `get_file_like(fd)` 把 `c_int` 转为表索引并查找 `FileDescriptor`。
2. 找不到时返回 `KError::BadFileDescriptor`。
3. `get_file_like_as<T>` 对 `Arc<dyn FileLike>` 执行 `downcast_arc`。
4. 类型不匹配返回 `KError::InvalidInput`。

### 获取 descriptor snapshot

1. 调用方持有 fd table 读锁。
2. `snapshot(fd)` 查找 `FileDescriptor`。
3. 找不到时返回 `KError::BadFileDescriptor`。
4. 找到时复制 fd 号、`cloexec`、对象级 `open_flags`，
   并克隆底层 `Arc<dyn FileLike>`。
5. 调用方释放 fd table 锁后仍可通过 snapshot 访问同一个 open object。

`FdSnapshot` 用于 `/proc/<pid>/fd/N`、`/proc/self/fd/N`、`fexecve`、
exec loader 等需要先稳定引用 open file 再进入 VFS 或装载路径的场景。
它不是 fd table 的 live view：
snapshot 创建后，原 fd 可以被关闭或复用，
但 snapshot 仍持有创建时的 open object 强引用。

### `dup2` / `dup3` 固定目标复制

1. 校验旧 fd 存在，克隆其 `FileDescriptor`。
2. 按调用参数设置新 descriptor 的 `cloexec`。
3. 移除目标 fd 旧条目。
4. 把克隆 descriptor 插入目标槽位。
5. 目标槽位越界时返回 `BadFileDescriptor`。

`duplicate_to` 假定 syscall 层已经处理 `old_fd == new_fd` 的 Linux 语义。
当前 `posix/fs` 的 `sys_dup2` 和 `sys_dup3` 在进入该函数前完成对应分支。

### close range

1. 获取当前最大已分配 fd。
2. 将请求区间裁剪到最大已分配 fd。
3. 按 fd 递增逐个 `remove`。
4. 不存在的槽位被忽略。

`sys_close_range` 在进入 `kfd` 前校验 `first >= 0` 和 `last >= first`。

### `Kstat` 到 Linux ABI

1. `From<kvfs::Metadata> for Kstat` 把 VFS 元数据转换为内核通用结构。
2. `From<Kstat> for stat` 先构造全零 Linux ABI 结构，
   再填充已知字段。
3. `From<Kstat> for statx` 同样先全零初始化，
   再设置 metadata、timestamp、device 和 regular-file atomic write 字段。

## 并发模型

`FdTable` 自身没有 interior locking。
共享 fd table 由 `Arc<RwLock<FdTable>>` 承载，
通常通过 `kresources` 进入。

锁策略：

- 查找 fd、读取 `cloexec`：调用方持读锁即可。
- 创建 `FdSnapshot`：调用方持读锁，克隆 `Arc<dyn FileLike>` 后释放锁。
- 添加、删除、dup、设置 `cloexec`、close range：调用方必须持写锁。
- `close_all_if_unshared` 在持写锁时移除 descriptor，
  把移除结果暂存在 `Vec` 中，
  然后先释放表锁再 drop `FileDescriptor`。

这样可以避免 `FileLike::drop` 或底层对象释放路径在持有 fd table 写锁时重入 fd 表。

`FileLike` trait 要求实现者满足 `DowncastSync`，
并通过各自内部锁保证对象级并发安全。
`kfd` 只管理 descriptor 到对象的引用关系，
不串行化具体文件对象的读写偏移或 socket 状态。
