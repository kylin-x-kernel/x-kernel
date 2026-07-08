# posix-mm — 安全与可靠性分析

## 信任模型

`posix-mm` 信任：

- 体系结构 fault/syscall 入口已把控制流切换到内核；
- `kprocess` 当前进程/线程状态可用；
- `kfs`、`memspace`、`filemap` 子系统各自维护内部不变量。

它自身负责：

- 校验用户传入参数；
- 将 raw syscall 参数翻译成 typed request；
- 正确装配 anonymous file 对象与 fd；
- 把底层错误转换成 syscall 结果。

## 外部边界 / 攻击面

- 用户提供的 `mmap` / `mremap` / `mprotect` / `munmap` /
  `madvise` / `msync` / `memfd_create` 参数
- 用户指针读取的 memfd 名字与其它 syscall 结构体
- 当前进程 fd table 与地址空间状态

## unsafe 代码清单

本 crate 没有直接 `unsafe` 代码块。

## 内存安全不变量

- raw syscall flags 必须先转换成 typed request，不能直接传入 `MmSpace`
  或 `filemap`。
- unknown flags 必须显式返回错误，不能通过 `from_bits_truncate()` 静默丢弃。
- Linux-known deferred `mmap` flags 必须留在 `posix-mm` 边界内处理：
  普通 mapping 可按兼容 no-op policy 通过，`MAP_SHARED_VALIDATE` 必须返回
  不支持。
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE` 地址必须按目标页大小对齐。
- successful `mremap` move must not retire the source with ordinary
  `MmSpace::unmap()`.
- `munmap(addr, 0)` 必须失败，不能作为成功 no-op 穿透到 `MmSpace`。
- `PROT_GROWSDOWN` / `PROT_GROWSUP` 必须在 syscall 边界拒绝。
- `MS_ASYNC | MS_SYNC` 冲突组合必须拒绝；unknown `msync` flags 也必须拒绝。
- `MS_INVALIDATE` 必须返回不支持，直到 locked VMA 与 invalidate 语义存在。
- `posix-mm` 不得直接访问 pagecache 或 filemap runtime internals；`msync`
  必须通过 `MmSpace::msync_range()` 进入 MM。
- `mremap` move must preserve the ownership split between VMA metadata,
  present PTE residency, and backing object pages.
- `memfd_create` 返回的文件对象必须持有有效 `Location`。
- anonymous file `Location` 必须绑定到 regular-file inode。
- anonymous file inode 的内容必须由 inode-owned `Mapping` 提供。
- `memfd_create` 必须通过 `memfs::shmem` factory 创建匿名文件，不能在 syscall 层
  复制 private tmpfs 文件创建、重新打开 anonymous location，或绕过 inode-scoped
  shmem policy state。
- `MFD_ALLOW_SEALING` 必须只影响初始 seal set：未设置时初始
  `F_SEAL_SEAL`，设置时初始空 seal set。
- `memfd` 名字读取必须有 Linux-compatible 上限，不能无界扫描用户内存。
- anonymous file 对象不应依赖全局路径唯一性。

## 线程安全

- fd 安装通过进程资源表同步。
- `memfs::shmem` state、VFS inode 与 `Mapping` 分别由各自锁保护。
- `posix-mm` 不额外缓存跨 syscall 的裸指针或未同步共享状态。

## 威胁分析

| 威胁 | 应对 |
|------|------|
| 用户通过 `memfd` 名字污染全局路径空间 | `memfs::shmem` factory 创建 private mount 上的 anonymous file |
| 用户构造包含 `/` 的名字影响 VFS 路径语义 | syscall 边界拒绝包含 `/` 的 memfd 名字 |
| 用户提供未终止或超长 memfd 名字 | `load_string_with_max_len()` 限制为 249 bytes excluding NUL |
| 用户通过未知 `memfd_create` flags 进入未定义路径 | `MemfdFlags::from_bits()` 拒绝未知 bits |
| raw `mmap` flags 静默穿透到 MM internals | `MmapRequest` 显式校验 unknown/deferred flags |
| unaligned fixed mmap 覆盖错误地址 | fixed policy 在 request 解析阶段拒绝未对齐地址 |
| `munmap(addr, 0)` 被当作成功 no-op | `MunmapRequest` 在 syscall 边界拒绝零长度 |
| `MADV_DONTNEED` 误伤 file source object | 当前只接 pure private anon 和 file-private anon result object |
| `msync` 把 private COW 页写回原文件 | `MmSpace::msync_range()` 只让 shared file runtime 执行 writeback |
| `MS_INVALIDATE` 被假实现成成功 | 当前返回 `OperationNotSupported` |
| `mremap` move 后旧源 teardown 销毁目标仍需的 private backing | move 分支只做 `map_relocated_snapshot` + `move_pages` + `drop_mapping_metadata` |

## 故障模式与影响分析（FMEA）

| 故障 | 条件 | 处理 | 影响 |
|---|---|---|---|
| 用户名字读取失败 | 无效用户指针、非法 UTF-8、缺少 NUL、超长 | 返回错误 | syscall 失败 |
| anonymous file 创建失败 | 内存不足或 VFS 分配失败 | 返回错误 | syscall 失败 |
| fd 安装失败 | fd table 满 | 返回错误 | 文件对象立即释放 |
| `mmap` flag 不支持 | unknown `MAP_*`、`MAP_SHARED_VALIDATE` 携带 deferred flag、或 file-backed hugetlb | 返回错误 | mmap 失败，不修改地址空间 |
| fixed mmap 地址未对齐 | `MAP_FIXED*` 地址不满足目标页大小 | 返回错误 | mmap 失败，不修改地址空间 |
| `munmap` 范围非法 | 长度为 0 或下游地址空间拒绝 | 返回错误 | 不修改地址空间 |
| `mprotect` grow flag 不支持 | `PROT_GROWSDOWN` 或 `PROT_GROWSUP` | 返回错误 | protect 失败，不修改 PTE/VMA |
| `MADV_DONTNEED` 范围非法 | 地址未映射、未页对齐、或 `addr + len` 溢出 | 返回错误 | advice 失败，不修改对象 |
| `msync` flag 不支持 | unknown flags、`MS_ASYNC | MS_SYNC`、或 `MS_INVALIDATE` | 返回错误 | sync 失败，不修改对象 |
| shared file writeback 失败 | VFS `AddressSpaceOperations::writepages()` 失败 | 返回错误，dirty bit 保留 | syscall 失败，调用者可重试 |

## 故障管理

- 参数和用户内存错误通过 `KResult` 返回。
- 不通过 panic 处理普通 syscall 失败。
- anonymous file 创建后若 fd 安装失败，由 `Arc` 生命周期自然回收对象。

## 已知限制

- `memfd_create` 支持 `MFD_ALLOW_SEALING` 初始 seal policy；
  `F_ADD_SEALS` / `F_GET_SEALS` 由 `posix-fs` 处理，shared writable mmap
  seal enforcement 由 `mm/filemap` 处理，hugetlb 扩展未接入。
- `memfd` 名字仅用于调试，不保证与 Linux `/proc/self/fd` 展示完全一致。
- `madvise` 支持 `MADV_DONTNEED`；其它 advice 返回 `InvalidInput`。
- `mmap` 的 maximum permission 与 current permission 分离；shared file
  映射的 may-permission 受文件打开权限约束。
- `msync` 支持 shared file dirty folio writeback；exact range fsync、
  `MS_INVALIDATE` 和 locked-page `EBUSY` 语义未实现。
- file-backed writable `MAP_SHARED` 由 `filemap` runtime 通过 write-fault
  dirty tracking 支持。

## 审计清单

- 新增 syscall flag 时是否同步更新 typed request 校验与文档。
- syscall 是否先构造 typed request，再调用 `MmSpace` / `filemap`。
- 是否仍存在 `from_bits_truncate()` 静默吞掉未知 ABI flags。
- fixed-address mapping 是否仍拒绝未对齐地址。
- `memfd_create` 是否仍不依赖 `/tmp` 或其它全局路径。
- shmem factory 是否总是返回 private mount 上的 regular-file inode。
- `memfd` regular-file inode 是否始终复用 inode-owned `Mapping`。
- `MADV_DONTNEED` 是否只命中 private-anon object，并通过 object-driven
  invalidate 主线完成 PTE zap。
