# memfs — 安全与可靠性分析
## 信任模型

`memfs` 信任 VFS 已完成路径解析和父目录权限检查，并信任 callback 传入的 `Cred`
是该次完整操作使用的稳定快照。它负责维护目录结构、inode
生命周期、符号链接内容，以及 regular-file inode 与 VFS-owned `AddressSpace`
（其私有 `PageCache` storage）的绑定关系。

## 外部边界 / 攻击面

- 文件创建、链接、重命名、删除
- regular-file read/write/truncate
- 匿名 tmpfs 文件对象创建
- `VfsInode::i_mapping` 暴露的 address-space/page-cache owner
- inode-owned `kvfs::AddressSpace` 暴露的 MM shared object identity
- create/mkdir/mknod/symlink callback 的 mode 与 `&Cred`
- `tmpfs` / `sysfs` nodev factory 创建的 superblock

## unsafe 代码清单

本 crate 不包含 `unsafe`。

## 内存安全不变量

- 同一 regular-file inode 最多拥有一个 `VfsInode::i_mapping`
  `AddressSpace`。
- 同一 inode `AddressSpace` 最多拥有一个私有 `PageCache` 实现组件。
- 所有指向同一 inode 的 open-file / mmap 路径都必须复用同一个 owner。
- 符号链接不能进入 regular-file page-cache 路径。
- 匿名文件不能挂入进程可见 mount namespace。
- 新 inode 的 UID/GID 必须来自 `inode_init_owner()`；setgid 父目录的 GID 与 mode
  传播必须在 inode 发布前完成。
- filesystem type factory 不得把 per-mount policy 写入共享 superblock flags。
- sysfs 的所有可见 mount 必须复用 `SYSFS` 中发布的同一 superblock，不能建立与 boot
  `/sys` 内容分离的第二棵树。
- sysfs singleton 必须保留 internal root mount 的 active 引用，不能让可见 mount 的
  卸载触发共享内核目录树 teardown。

## 线程安全

- inode metadata、目录项和 mapping state 都通过对应 owner 的锁保护。
- regular-file content ownership 位于 `VfsInode::i_mapping` 下，不引入第二套
  file-backed content ownership。
- `kvfs::AddressSpace` 只通过 `Arc` 传播，不暴露共享裸指针或内部
  `PageCache` storage。
- sysfs singleton 的 superblock 与 internal mount 由同一个 `Once` 发布，首次构造完成前
  不会返回半初始化对象。

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
   - 防护：`TMPFS_TYPE` / `SYSFS_TYPE` factory 只接收 VFS-wide `SuperBlockFlags`；调用者在
     attach 时另行设置每个 mount 的策略。

7. 第二次挂载 sysfs 得到空白的独立目录树。
   - 防护：`new_sysfs(SuperBlockFlags)` 和 `SYSFS_TYPE` factory 读取同一个
     `Once<(Arc<SuperBlock>, Arc<Mount>)>`；internal mount 保持 shared tree active。

## 故障模式与影响分析（FMEA）

| 故障 | 条件 | 处理 | 影响 |
|---|---|---|---|
| folio 分配失败 | 内存不足 | 返回 `NoMemory` | 文件读写或 mmap fault 失败 |
| inode owner 缺失 | attachment 初始化失败 | 返回错误 | 避免 silent corruption |
| 同 inode 多 entry 未共享 owner | VFS inode identity 传播错误 | 审计和测试捕获 | 读写/mmap 一致性破坏 |

## 故障管理

- 读写和 truncate 错误通过 `VfsResult` 返回。
- inode/mapping 绑定缺失必须返回错误，不能静默创建不一致的第二对象。

## 已知限制

- 匿名文件通过私有 tmpfs mount + regular file 表达，不提供独立的 VFS
  anonymous-inode 类型。
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
- sysfs mount 是否始终复用 `new_sysfs(SuperBlockFlags)` 发布的 superblock。
