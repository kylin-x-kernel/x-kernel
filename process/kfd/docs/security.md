# kfd - 安全与可靠性分析

## 概述

`kfd` 负责把用户可见的整数 fd 映射到内核对象。
它包含少量 Rust `unsafe` 代码，
用于把 `Kstat` 转换为 Linux ABI 结构时初始化全零 `stat` / `statx`。
其余风险主要是资源生命周期、fd 表并发、descriptor flag 语义、
类型 downcast 和用户态 ABI 字段泄露。

## 信任模型

```text
syscall layer
   │ validates syscall-specific fd rules and flags
   │
   v
kresources
   │ owns Arc<RwLock<FdTable>>
   │
   v
┌─────────────────────────────────────────────┐
│ kfd                                        │
│                                             │
│ safe API                                   │
│  ├─ FdTable add/get/remove/dup/close       │
│  ├─ FileLike read/write/stat/ioctl/mmap    │
│  └─ Kstat -> stat/statx conversion         │
│                                             │
│ unsafe boundary                            │
│  ├─ zeroed::<linux_raw_sys::stat>()        │
│  └─ zeroed::<linux_raw_sys::statx>()       │
└──────────────────┬──────────────────────────┘
                   │ Arc<dyn FileLike>
                   v
VFS / socket / pipe / epoll / timerfd / eventfd implementations
```

- syscall 层负责 syscall 参数语义，
  例如 `close_range` 的区间合法性、`dup2` 的 `old_fd == new_fd` 特例。
- `kresources` 负责持有 fd table 的 `RwLock`。
- `kfd` 信任具体 `FileLike` 实现维护对象内部并发和 I/O 语义。
- `Kstat` 转换信任 `linux_raw_sys` ABI 结构可以被全零初始化。

## unsafe 代码清单

### 1. `From<Kstat> for stat` 中的 `core::mem::zeroed`

位置：`src/stat.rs:79`

```rust
let mut stat: stat = unsafe { core::mem::zeroed() };
```

不变量：

- `linux_raw_sys::general::stat` 是 plain-old-data ABI 结构。
- 全零值对所有字段有效：
  整数字段为 0，保留字段为 0。
- 结构初始化后只通过整数赋值填充，
  不包含需要 drop 的 Rust 引用或枚举 niche。

为何安全：

- `stat` 来自 `linux_raw_sys` 的 C ABI 绑定。
- 代码随后显式填充 syscall 需要返回的字段。
- 保留字段保持 0，避免未初始化内核栈内容泄露给用户态。

调用者：

- `posix/fs` metadata syscall 路径。
- 任何需要把 `Kstat` 写回用户态 `struct stat` 的 file-like 对象。

### 2. `From<Kstat> for statx` 中的 `core::mem::zeroed`

位置：`src/stat.rs:107`

```rust
let mut statx: statx = unsafe { core::mem::zeroed() };
```

不变量：

- `linux_raw_sys::general::statx` 是 plain-old-data ABI 结构。
- 全零值对所有字段有效。
- reserved 字段必须为 0，
  未支持的 attribute 和 mask 位不得包含未初始化数据。

为何安全：

- `statx` 来自 `linux_raw_sys` 的 C ABI 绑定。
- 转换路径填充 block size、uid/gid、mode、inode、size、blocks、
  rdev、时间戳、device major/minor 和 regular-file atomic write 字段。
- 未支持字段保持 0，符合 Linux ABI 对保留字段的要求。

调用者：

- `statx` syscall 路径。
- file-like metadata 查询路径。

## 内存安全不变量

1. **fd 槽位持有强引用**：
   每个 `FileDescriptor` 保存 `Arc<dyn FileLike>`，
   只要 fd 存在，底层对象不会释放。
2. **dup 共享 open object**：
   `duplicate_to` 克隆 `FileDescriptor`，
   共享同一个 `Arc<dyn FileLike>`，
   不复制底层对象。
3. **fd table 修改独占**：
   所有插入、删除和 flag 修改都要求调用方持有 `RwLock<FdTable>` 写锁。
4. **drop 不在表锁内执行**：
   `close_all_if_unshared` 先从表中取出 descriptor，
   释放写锁后再 drop，降低析构重入风险。
5. **ABI 结构不泄露未初始化数据**：
   `stat` / `statx` 转换先全零初始化，
   再填充字段。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `FdTable` | 由字段决定；通常包装在 `Arc<RwLock<_>>` 中跨线程共享 | 本身不提供同步；共享访问必须经外层 `RwLock` |
| `FileDescriptor` | `Arc<dyn FileLike>` 可发送时可发送 | 共享读取 descriptor flag 需要外层锁 |
| `dyn FileLike` | 实现者必须满足 `DowncastSync` | 对象内部读写状态由实现者自行加锁 |
| `Kstat` | plain metadata struct | plain metadata struct |

`kfd` 不保证具体 file-like 对象的读写原子性。
例如文件偏移、socket 状态和 pipe buffer 都由各自实现维护。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | fd 表越界插入覆盖其他资源 | 高 | `add_at` 接受超过 `FILE_LIMIT` 的目标 fd | `FlattenObjects` 限制容量；失败映射为 `BadFileDescriptor` |
| T-02 | 负 fd 经 `as usize` 转换为巨大索引 | 中 | syscall 层漏掉负 fd 校验 | 巨大索引查找失败；多数路径返回 `BadFileDescriptor`；syscall 层仍应保留语义校验 |
| T-03 | `max_nofile` 未执行导致突破进程资源限制 | 中 | 启动预装之外的路径直接调用 `insert_file_like` | 普通 syscall 路径使用 `add_file_like`；`insert_file_like` 保留给 stdio 预装和已授权路径 |
| T-04 | `dup2` / `dup3` 错误处理 `old_fd == new_fd` | 中 | 上层直接调用 `duplicate_to` | `posix/fs` 在 syscall 层处理特例；`duplicate_to` 文档记录调用前提 |
| T-05 | `cloexec` 未关闭导致 fd 泄露到 exec 后程序 | 中 | exec 路径未调用 `close_cloexec_files` 或 flag 未设置 | `close_cloexec_files` 统一收集并关闭；新增 exec 路径需调用 |
| T-06 | 关闭 fd 时底层对象析构重入 fd table | 中 | drop 在持有 fd table 写锁时触发资源回收 | `close_all_if_unshared` 释放表锁后 drop 移除项；其他 close 路径调用方需避免持锁执行复杂析构 |
| T-07 | downcast 类型错误被当成合法对象使用 | 中 | 调用者用错误的 `T::from_fd` 类型 | `downcast_arc` 失败返回 `InvalidInput` |
| T-08 | `stat` / `statx` 未初始化字段泄露内核内存 | 高 | 直接构造 ABI 结构但未清零 reserved 字段 | 转换先 `zeroed()`，保留字段保持 0 |
| T-09 | 低层槽位 API 被外部绕过资源策略或 descriptor flag 规则 | 中 | `add`、`add_at`、`remove`、`get_mut` 作为跨 crate API 暴露 | 已将这些 helper 收窄为 `pub(crate)`；外部路径使用高层 API |
| T-10 | `FileLike` 默认方法返回值掩盖不支持操作 | 低 | 具体实现未覆盖 read/write/ioctl/mmap | 默认返回 `InvalidInput`、`NotATty` 或 `NoSuchDevice`；调用者按 errno 处理 |
| T-11 | `Arc::strong_count` 判断期间出现新的共享者 | 中 | `close_all_if_unshared` 与 fd table 引用复制并发 | 调用方的进程资源替换路径应串行化 fd table Arc 的发布；函数只在 strong count 为 1 时关闭 |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `add_file_like` 返回 `TooManyOpenFiles` | 当前 fd 数达到 `max_nofile` 或表已满 | 当前 open/socket/pipe 失败 | 进程无法继续打开新 fd | 3 | 返回 `KError::TooManyOpenFiles` |
| F-02 | `get_file_like` 返回 `BadFileDescriptor` | fd 不存在、已关闭或为负 fd 转换后的无效索引 | 当前 syscall 失败 | 应用收到 `EBADF` | 4 | 查找失败显式返回错误 |
| F-03 | `get_file_like_as` 返回 `InvalidInput` | fd 存在但类型不匹配 | typed syscall 失败 | 应用收到类型相关错误 | 4 | 使用 `downcast_arc` 检查类型 |
| F-04 | `duplicate_to` 目标槽位插入失败 | 目标 fd 越界 | dup 操作失败 | 应用收到 `EBADF` | 4 | `add_at` 失败映射为错误 |
| F-05 | `close_range` 请求范围很大 | 用户传入接近 `i32::MAX` 的 last | 遍历到当前最大 fd 后停止 | CPU 开销受已分配最大 fd 限制 | 4 | 使用 `ids().next_back()` 裁剪上界 |
| F-06 | `close_cloexec_files` 关闭过程中 fd 表变化 | 调用方未持写锁 | 关闭集合不一致 | exec 后 fd 泄露或误关 | 2 | 资源层必须持写锁调用 |
| F-07 | `statx` regular-file atomic write 字段误报 | `mode` 类型位错误 | 用户态看到错误 attribute | 应用可能选择错误 I/O 策略 | 4 | 仅当 `mode & S_IFMT == S_IFREG` 时设置 |
| F-08 | FileLike 实现忘记覆盖 `path` 以外的方法 | 默认方法被调用 | 返回不支持错误 | 功能降级 | 4 | trait 默认错误返回；实现者测试覆盖自身行为 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- fd 不存在统一返回 `KError::BadFileDescriptor`。
- fd 表容量或进程软限制耗尽返回 `KError::TooManyOpenFiles`。
- typed downcast 失败返回 `KError::InvalidInput`。
- 默认 `FileLike` 方法对不支持操作返回明确错误：
  `read` / `write` 为 `InvalidInput`，
  `ioctl` 为 `NotATty`，
  `mmap` 为 `NoSuchDevice`。
- `close_range` 和 `set_cloexec_range` 忽略不存在的 fd，
  符合批量操作容忍 sparse fd 表的需求。

## 隐私分析

`kfd` 不直接读取用户 payload，
但 fd 表揭示进程打开了哪些内核对象，
`FileLike::path` 可能返回路径字符串，
`Kstat` 包含 uid/gid、inode、设备号和时间戳。
本 crate 不记录日志、不持久化这些信息。
向用户态返回 `stat` / `statx` 时必须保证 reserved 字段清零，
避免泄露内核栈或未初始化内存。

## 已知限制

1. **`insert_file_like` 不应用资源限制**：
   仅适合内部预装或已授权路径。
2. **fd 参数边界依赖 syscall 层**：
   `kfd` 多数接口把 `c_int` 直接转为 `usize` 查表。
3. **`FileLike` errno 语义由实现者补充**：
   trait 默认方法只描述通用不支持操作，
   具体对象仍需在自身 crate 中说明 read/write/ioctl/mmap 的 errno 细节。
4. **`close_all_if_unshared` 依赖发布侧串行化**：
   `Arc::strong_count` 只能表达当前引用数，
   不能替代进程资源层对 fd table 替换的同步。

## 其它说明（模板章节）

| 章节 | 说明 |
|------|------|
| 基线 | 以本仓库 `docs/templates/module-docs-guide.md` 及 `AGENTS.md` 为准 |
| 冗余设计 | 无 |
| 过载控制 | fd 数量由 `max_nofile` 和 `FILE_LIMIT` 控制 |
| 人因差错 | 无直接用户交互 |
| 故障预测预防 | 无 |
| 升级不中断业务 | 无 |

## 审计清单

修改 `kfd` 时需验证：

- [ ] 新增 `unsafe` 块附有 `SAFETY:` 注释并补入本文件 unsafe 清单。
- [ ] 新增 fd 插入路径明确是否应用 `max_nofile`。
- [ ] 新增 close/dup 路径在持锁、drop 和 descriptor flag 语义上符合 POSIX。
- [ ] 新增 `FileLike` 默认方法不会把不支持操作伪装成成功。
- [ ] `stat` / `statx` ABI 结构新增字段时保留字段仍清零。
- [ ] exec 路径继续调用 `close_cloexec_files`。
- [ ] 资源层替换 fd table 时保持 `Arc<RwLock<FdTable>>` 发布同步。
