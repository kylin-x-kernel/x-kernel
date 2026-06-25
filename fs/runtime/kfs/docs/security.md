# kfs — 安全与可靠性分析

## 信任模型

`kfs` 信任底层 `kvfs` 节点实现维护 inode identity、mount flags 和 node
operation contracts。调用方可能来自 syscall、loader、VFS mount setup 或内核
任务，因此高层 API 必须在边界检查权限、mount writability 和 file access flags。

## 外部边界 / 攻击面

- 用户可通过 syscall 间接驱动 `open`、`read`、`write`、`truncate`、`mmap`
  和 `memfd_create`。
- `memfd` 名字来自用户内存，但在 `posix-mm` 边界完成长度和字符串校验。
- shmem object state 通过 inode data 共享，所有同 inode alias 都会观察同一
  policy state。

## unsafe 代码清单

`kfs` 当前高层代码不直接包含新的 `unsafe` 块。folio storage 的 slice 构造由
`mm/pagecache` 拥有并在该 crate 的安全文档中说明。

## 内存安全不变量

- 同一 regular-file inode 最多拥有一个 `VfsInode::i_mapping`
  `AddressSpace`，该 address-space 内最多拥有一个 live page-cache
  `Mapping`。
- KFS 不能创建 open-file-scoped cache identity；所有 regular-file cache identity
  必须来自 `VfsInode::i_mapping -> AddressSpace`。
- page-cache eviction listener 通过 RAII `EvictRegistration` 注册；guard drop
  必须自动 unregister，不能要求调用者手动保存整数 id。
- tmpfs/memfs inode 的 page-cache mapping 必须使用
  `MappingKind::InMemory`。
- shmem policy state 必须挂在 inode data 上，不能挂在 open-file 实例上。
- anonymous shmem file 必须是 regular-file inode，不能是 directory、device
  或 stream node。
- `pagecache::Mapping` 不解释 memfd seals；seal checks 必须在 KFS/filemap/
  memspace policy 边界执行。
- KFS write、append 和 set_len 路径必须先执行 shmem seal policy check，
  再修改文件内容或长度。
- KFS 必须向 `mm/filemap` 暴露 inode-scoped shared writable mapping policy：
  `F_SEAL_WRITE` 拒绝新 shared writable mappings、保护升级和 write fault；
  `F_SEAL_FUTURE_WRITE` 只拒绝新 shared writable mappings 和保护升级。

## 线程安全

- inode data 插入由 VFS inode attachment lock 保护。
- address-space page cache、evict listener list 和 shmem seal bitmask 使用
  sleepable锁，调用路径允许阻塞。
- 不能在持有 spinlock 或 IRQ-disabled guard 时调用 highlevel file/shmem API。

## 威胁分析

| 威胁 | 应对 |
|------|------|
| `memfd` 名字污染全局路径空间 | `memfs::shmem` 创建 private mount 上的 anonymous file |
| 同 inode alias 看到不同内容 | 唯一 cache identity 挂在 `VfsInode::i_mapping -> AddressSpace` |
| seal policy 被 open-file clone 绕过 | `ShmemObjectState` 挂在 inode data 上 |
| seal 状态混入通用 page cache | pagecache 保持 seal-unaware，KFS/filemap 执行 policy |
| anonymous file 不是 regular inode | factory 只调用 memfs regular-file 创建路径 |
| sealed shmem 文件通过 direct/page-cache 路径绕过限制 | `File` 在 write/resize 边界执行 shmem policy check |

## 故障模式与影响分析（FMEA）

| 故障 | 条件 | 处理 | 影响 |
|---|---|---|---|
| anonymous file 创建失败 | 内存不足或 VFS 分配失败 | 返回错误 | syscall/调用方失败 |
| shmem state 插入失败 | inode data 已有同类型 state | 复用已有 state | 同 inode policy 保持一致 |
| page-cache folio 分配失败 | 内存不足 | 返回错误 | read/write/mmap fault 失败 |
| address-space writeback 失败 | backing filesystem 返回错误 | 返回错误 | 调用方可重试 |

## 故障管理

普通失败通过 `VfsResult`/`KResult` 返回。`kfs` 不用 panic 表示用户可触发的
文件操作失败。

## 已知限制

- `ShmemObjectState` 已保存 seal bitmask，并通过 `posix-fs`
  `F_ADD_SEALS` / `F_GET_SEALS` 暴露给用户态。
- KFS 执行 write/resize seal checks，并为 `mm/filemap` 提供 shared
  writable mmap、`mprotect(PROT_WRITE)` 和 shared write-fault checks。
- KFS 保存 active shared page 计数和 active writable shared page 计数，用于在
  `F_ADD_SEALS` 添加 `F_SEAL_WRITE` 时拒绝仍有 writable shared mapping 的
  memfd/shmem object。
- SysV shm 通过 `memfs::shmem::create_kernel_file()` 创建 shmem file，并通过
  file-backed shared mapping 路径共享内容。
- tmpfs quota、swap、THP、xattr 和完整 Linux inode accounting 不在当前
  shmem factory 中实现。

## 审计清单

- 新 shmem 用户是否通过 `memfs::shmem` tmpfs/shmem inode factory，
  而不是直接复制 private `memfs` 创建逻辑。
- shmem policy state 是否保持 inode-scoped。
- regular-file cache identity 是否仍只来自 `VfsInode::i_mapping`。
- file write/truncate/mmap/mprotect seal enforcement 是否统一读取同一
  `ShmemObjectState`。
- `F_SEAL_WRITE` 与 active writable shared mappings 的 Linux 完整互斥检查
  是否持续由 filemap runtime 注册和注销 active writable shared pages。
- 是否避免在中断上下文调用 highlevel file/shmem API。
