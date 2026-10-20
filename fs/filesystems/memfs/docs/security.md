# memfs — 安全与可靠性分析

## 测试审计范围

审计范围包含生产源码及 `ut-tests/lib.rs`、`ut-tests/shmem.rs`、
`ut-tests/quota.rs` 的 guest 用例和 fixture。测试仍为原父模块的私有子模块，
仅在 `cfg(unittest)` 下编译；提取 quota 测试不改变容量计数、失败回滚、
生产可见性或现有 lint 例外。三组测试没有新增 unsafe 或 host 注册。

## 信任模型

`memfs` 信任 VFS 已完成路径解析和父目录权限检查，并信任 callback 传入的 `Cred`
是该次完整操作使用的稳定快照。它负责维护目录结构、inode
生命周期、符号链接内容，以及 regular-file inode 与 VFS-owned `AddressSpace`
（其私有 `MappingPages` storage）的绑定关系。

## 外部边界 / 攻击面

- 文件创建、链接、重命名、删除
- regular-file read/write/truncate
- 匿名 tmpfs 文件对象创建
- `VfsInode::i_mapping` 暴露的 address-space/page-cache owner
- inode-owned `kvfs::AddressSpace` 暴露的 MM shared object identity
- create/mkdir/mknod/symlink callback 的 mode 与 `&Cred`
- Superblocks created by the `tmpfs` nodev factory.
- tmpfs mount data, including `size` / `nr_blocks`, root metadata options, and
  remount size changes.

## unsafe 代码清单

本 crate 不包含 `unsafe`。

## 内存安全不变量

- 同一 regular-file inode 最多拥有一个 `VfsInode::i_mapping`
  `AddressSpace`。
- 同一 inode `AddressSpace` 最多拥有一个私有 `MappingPages` 实现组件。
- 同一 memfs backing inode number 必须以所属 `SuperBlock` 为联合键进入 VFS-wide identity
  table 并取得唯一
  `VfsInode`；`MemoryFs` 不得保存第二张 VFS inode cache。
- 所有指向同一 inode 的 open-file / mmap 路径都必须复用同一个 owner。
- 符号链接不能进入 regular-file page-cache 路径。
- 匿名文件不能挂入进程可见 mount namespace。
- 新 inode 的 UID/GID 必须来自 `inode_init_owner()`；setgid 父目录的 GID 与 mode
  传播必须在 inode 发布前完成。
- filesystem type factory 不得把 per-mount policy 写入共享 superblock flags；
- mounted tmpfs quota 只按实际 resident folio 计数，不得把文件逻辑长度或 sparse
  hole 当作已用页；buffered read 的 hole 直接零填充、不 materialize、不 charge；
  mmap 读缺页仍走 folio materialization 并 charge quota；
- quota 不预留物理内存；remount 不得降到当前用量以下，也不得把 unlimited 实例
  追溯改成有限，但不得截断或丢弃已有文件数据；
- `size=0` / `nr_blocks=0` 是显式 unlimited。`statfs()` 对 unlimited 报 0 个
  block 是 Linux 兼容行为，不代表容量为零或写入必须失败；
- mount-data parser 只接受本文件系统明确支持的选项并校验取值；`mode`、`uid`、
  `gid` 用于构造 root metadata；`nr_inodes=0` 表示 unlimited，非零值因
  尚无有限 inode 计账而返回 Unsupported，不能成功接受无效上限；`noswap` 因无
  swap 后端而作为 no-op 接受；
- quota charge/release 必须由 mapping ownership 驱动：folio 退出 cache 时释放，
  memfs 不得维护第二套 page accounting。

## Thread Safety

- inode metadata、目录项和 mapping state 都通过对应 owner 的锁保护。
- root 与后续 lookup/create/link 使用同一 VFS-wide identity table 和 superblock key；新 identity 在可见前
  已绑定该 superblock。
- regular-file content ownership 位于 `VfsInode::i_mapping` 下，不引入第二套
  file-backed content ownership。
- `kvfs::AddressSpace` 只通过 `Arc` 传播，不暴露共享裸指针或内部
  `MappingPages` storage。
- Mount capacity policy lives in memfs `quota.rs`. Its lifecycle provider is
  fixed during inode construction and retained once by each mapping.
- Admission/removal run under the cache index mutex and may take the quota limit
  lock; they never enter mappings or lock folios. Remount/statfs never acquire
  cache locks. One limit snapshot supplies both statfs total and free pages.
- Every winning identity is charged once, and removal returns capacity even if
  an external folio reference survives. No page or entry carries a quota Arc.
- Sparse-hole handling belongs to memfs's read operation. Cache access retries
  retained initialization errors and excludes truncate through EOF/data copying.

## 威胁分析

1. 硬链接路径得到不同内容对象。
   - 防护：不同 dentry 共享同一个 `VfsInode::i_mapping` address-space。

2. 截断后内容残留。
   - 防护：`AddressSpace::truncate_setsize()` 先发布 inode size，再通过私有 cache storage
     清零 surviving tail，并丢弃 EOF
     之后 folio。

3. 符号链接错误进入 page-cache 路径。
   - 防护：symlink target 保存在 `FileContent.symlink`，不通过
     regular-file page-cache data path。

4. 匿名文件对象意外暴露到全局路径空间。
   - 防护：`memfs::shmem` 使用私有 `MemoryFs("tmpfs")` root mount 创建
     regular file，并且不把该 mount 挂入进程可见命名空间。

5. 新 inode 错误继承 root 或调用者 effective identity。
   - 防护：所有创建 callback 使用显式 `&Cred` 调用 `inode_init_owner()`，普通创建取
     `fsuid/fsgid`，setgid 父目录继承父 GID。

6. nodev factory 把 `nosuid/nodev/noexec` 固化到 filesystem instance。
   - Mitigation: The `TMPFS_TYPE` factory accepts only VFS-wide
     `SuperBlockFlags`; callers apply per-mount policy when attaching the mount.

7. 未受限制的 tmpfs 写入把所有物理页变成不可丢弃的 InMemory 数据。
   - 防护：mounted tmpfs 默认使用 allocatable physical pages 的 50% 作为
     page quota；quota 满时 materialization 返回 `StorageFull`。

8. quota 计数与实际 folio 生命周期分离。
   - 防护：quota charge/release 由 page-cache mapping ownership 驱动；truncate、
     final eviction 和插入失败使用同一路径，并有回归测试。

9. memfd 因模式过窄被 exec 拒绝（runc/crun self-reexec）。
   - 防护：`create_memfd_file()` 创建 `0o777` 文件；exec 的 `MAY_EXEC` 因此通过。
     显示路径 `/proc/self/fd/<n>` 与 `/proc/self/exe` 指向同一 opened backing file，
     不引入可写可执行的内存映射。

10. memfd seal 语义偏差。
   - 防护：只允许添加 seal（`F_SEAL_SEAL` 之后拒绝一切新 seal）；
     `MFD_ALLOW_SEALING` 未设置时创建即打 `F_SEAL_SEAL`，对齐 Linux。

## 故障模式与影响分析（FMEA）

| 故障 | 条件 | 处理 | 影响 |
|---|---|---|---|
| tmpfs quota 已满 | 新 folio materialization 请求超过上限 | 返回 `StorageFull` | 用户态写入收到 `ENOSPC`，已有数据保持可用 |
| tmpfs remount 限额过小 | 新有限上限低于当前用量 | 返回 `InvalidInput` | 用户态收到 `EINVAL`，原限额和已用页数不变 |
| 候选 folio 分配失败 | 内存不足 | 先重查并发 resident；没有可复用页时返回 `NoMemory`，此时尚未 charge | 文件读写或 mmap fault 失败 |
| inode owner 缺失 | attachment 初始化失败 | 返回错误 | 避免 silent corruption |
| 同 inode 多 entry 未共享 owner | VFS inode identity 传播错误 | 审计和测试捕获 | 读写/mmap 一致性破坏 |

## 故障管理

- 读写和 truncate 错误通过 `VfsResult` 返回。
- 内存文件的同步成功只表示无需持久化 I/O，不承诺断电保存；同步不得丢弃内容页。
  特殊文件的同步能力由打开后安装的专用 operation table 决定。
- inode/mapping 绑定缺失必须返回错误，不能静默创建不一致的第二对象。
- quota admission 的容量拒绝作为文件系统 `StorageFull` 错误传播。候选物理页和
  heap 对象分配发生在 admission 前，仍遵循各自的内存分配失败路径。

## 已知限制

- 匿名文件通过私有 tmpfs mount + regular file 表达，不提供独立的 VFS
  anonymous-inode 类型。
- mounted tmpfs quota 不提供 swap-out；无 swap 时已 resident 的 InMemory 数据仍
  不能被通用 clean-page reclaim 丢弃。
- anonymous shmem/memfd instance 目前不套用 mounted tmpfs 的 size 默认值。
- resident-folio quota 不覆盖 inode/dentry 元数据和 symlink target String（包括长目标），
  也不严格限制未发布候选页或已脱离 mapping 的外部物理引用。
- `memfs` regular-file 数据路径依赖 KFS `AddressSpace` 及其私有 page-cache storage；
  `MemoryNode` 自身不直接保存普通文件数据页。

## 审计清单

- regular-file inode 是否总能复用同一个 `VfsInode::i_mapping`。
- `lookup/create/link` 生成的新路径是否仍命中同一个 inode address-space。
- 同一 inode 的 MM 路径是否复用同一个 `kvfs::AddressSpace`。
- 符号链接路径是否完全绕开 regular-file page cache。
- 匿名文件 helper 是否始终返回未挂载到全局命名空间的对象。
- 所有 inode 创建路径是否接收 `&Cred` 并在发布前调用 `inode_init_owner()`。
- setgid 父目录的普通 child GID 和子目录 setgid bit 是否有测试覆盖。
- filesystem type factory 是否只创建 superblock，而不决定 per-mount flags。
- tmpfs 默认值是否为 allocatable physical pages 的一半，且未预留物理内存。
- `size`/`nr_blocks` 解析是否拒绝未知或 malformed 选项，`0` 是否表达 unlimited。
- quota 是否按实际 folio 计数，truncate/final eviction 是否释放容量。
- `statfs` 是否报告 quota 总量和剩余量。

Mounted tmpfs accepts `nr_inodes=0` as unlimited; nonzero inode limits remain
unsupported. A file-backed fault that exhausts resident capacity becomes
`BusError` and delivers `SIGBUS`; ordinary buffered writes still return `ENOSPC`.

Ordinary reads and copy_file_range use the same filesystem-owned sparse reader.
A source hole must neither increase source nrpages nor consume mount capacity,
even when native copy writes into an existing destination page at full quota.
Each bounded batch retains invalidation shared across EOF checks and copying;
only one folio is locked at a time. Initialization failures remain errors and
cannot become holes. No StorageFull error is ignored to emulate sparse reads.
