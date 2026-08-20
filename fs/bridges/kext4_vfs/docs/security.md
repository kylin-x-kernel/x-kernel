# kext4_vfs — 安全与可靠性分析

## 信任模型

Bridge 信任 KVFS 提供有效的 kernel-owned `VfsInode`、dentry、PageCache 和生命周期 callback，
信任 `kext4` core 已校验 ext4 磁盘元数据。来自 syscall 的 name、offset、length、metadata update
和 credential 仍是边界输入，必须在 KVFS/core 对应边界校验。

## 外部边界 / 攻击面

- KVFS create/link/unlink/rename/truncate/read/write/fsync/syncfs callback；
- PageCache writeback、invalidate、reclaim 和 final eviction 顺序；
- 块设备 I/O 与 journal/recovery 错误，由 core 以 `Ext4Error` 返回；
- canonical `FileSystemType -> get_tree_bdev -> fill_super` 挂载入口；
- UID/GID/mode 和 device id 的 VFS/core 转换。

Bridge 不直接访问 user pointer、MMIO/PIO、DMA、firmware、FFI 或 inline assembly。

## unsafe 代码清单

无。当前 crate 源码没有 `unsafe` block；底层 KVFS、block device 和 KExt4 core 是其安全边界。

## 内存安全不变量

- 每个 bridge `Inode` 必须组合持有该 `VfsInode` 唯一的 ext4 private state，且 VFS inode
  number 必须与 private state number 相同。
- KVFS generic attribute operations 必须直接落到同一个 inode component；bridge 不得保存或
  mutation 后回灌第二份 mode/owner/nlink/time/size/block snapshot。
- KVFS-wide `(SuperBlock, ino)` table 是唯一 resident identity table；`SuperBlock`、bridge filesystem 和
  KExt4 不得再按 inode number 缓存、合并或驱逐 private state。
- ext4 只能由自己的 `register_init` 回调注册一个静态 `FileSystemType`；创建出的
  `SuperBlock.s_type` 必须引用同一对象，root 和普通 mount 不得另建 provider、名称字段或
  第二个 device-mount 入口。
- 同一 ext4 type 与 `dev_t` 的实例身份由 KVFS `sget_dev` 等价 registry 拥有；bridge
  `fill_super` 只能给 VFS 已分配的新生 `SuperBlock` 安装私有 operations 与 root，不得另建
  mount cache、复制 identity 字段或重复打开同一设备状态。
- zero-link inode 的 PageCache、data/xattr blocks 和 inode bitmap 只能在最后 `VfsInode` 引用
  drop 的 superblock hook 中释放。
- delalloc interval、per-inode reserved count 和 mount aggregate 必须由一个 core range API 在
  mount mutation guard 下联合更新；bridge 不得直接增减其中任何一项。
- PageCache traversal 期间不得持有 core write guard，避免 mapping/core 锁序反转。
- Cross-filesystem private state 是内部契约失败并映射为 `EIO`；当前运行态没有可逃逸的 core
  inode handle。`Freeing` 由 KVFS 在内部等待重试，不进入普通 core API；未来若引入带
  generation 的外部 handle，其真实失效才应映射 `ESTALE`。

## 线程安全

挂载级 `RwLock` 保护 core mutation 入口，per-inode `writeback_lock` 串行化 writeback pass，KVFS
VFS-wide Weak cache 按 superblock 与 inode number 合并 identity。只读 guard 可并发；mutation 仍串行。KVFS cache mutex 不跨
等待或 filesystem callback 持有；KExt4 没有第二把 inode-cache mutex。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | unlink 后过早释放仍打开的 inode | 中 | namespace removal 与 file/VFS 引用生命周期混淆 | unlink 只更新 dirent/nlink/orphan；open file 继续持有唯一 `VfsInode`，最后 VFS 引用 drop 才调用 final eviction |
| T-02 | bridge 重载 inode snapshot 导致 metadata/transient state 分叉 | 中 | callback 只保存 number，或 core namei 重复 decode live child | `Inode` 组合持有 private state；unlink/rmdir/rename 从 locked dentry 取得并传入既有 victim/moved/replaced state；普通 iget 只在 KVFS `New` initializer 中 decode |
| T-03 | eviction 普通能力泄漏或误删重用后的 cache identity | 高 | final hook 开始后普通 lookup 获得旧 inode，或 finish 只按 number 删除 | KVFS 在 hook 前精确匹配对象并置 `Freeing`；lookup 等待；三个 core eviction phase 只借用 hook 持有的同一 private component；finish 只删除匹配旧对象的 entry 并唤醒重试 |
| T-04 | delalloc accounting 漂移导致过量预留或 statfs 错误 | 中 | per-inode interval/count 与 mount aggregate 分开更新，或只按 `i_disksize` 判断 truncate | reserve/release/truncate/writeback/eviction 复用 core range API，一次更新 interval、per-inode count 和 mount aggregate；EOF tail 释放按旧 `i_size` 判断，磁盘 mapping shrink 单独按旧 `i_disksize` 判断；bridge 无 reservation counter |
| T-05 | PageCache/core 锁反转造成永久等待 | 中 | writeback 持 core lock 遍历 folio，同时 read fault 持 mapping lock 进入 core | traversal 外不持 core lock；batch callback 在 mapping/folio lock 释放后进入 core；per-inode writeback lock 单独串行化 pass |
| T-06 | shrink 后重新增长暴露旧缓存数据 | 高 | backing prepare 提前发布新 `i_size`，使通用 PageCache 看不到旧 EOF | core prepare 只提交 `i_disksize`；bridge 恰好调用一次 `truncate_setsize()` 发布 `i_size` 并完成 folio/mmap 处理；回归测试覆盖 shrink、shrink-regrow 与 partial-page grow |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | core mutation/eviction 返回错误 | I/O、journal abort、corruption 或 internal cross-filesystem contract | 当前 VFS operation 失败 | mount 可能保持 recovery evidence 或拒绝后续 mutation | 2 | 跨文件系统 private component 是 `EIO` 级内部合同失败；`Freeing` 不离开 KVFS cache；final eviction 记录 warning，磁盘一致性依赖 core journal/recovery 证据 |
| F-02 | 粗粒度 core write guard 阻塞 | 慢 I/O 或 checkpoint | 同 mount mutation 排队 | 吞吐下降 | 4 | read path 共享 guard；writeback 不跨 PageCache traversal 持 guard；后续只按实测锁域继续拆分 |
| F-03 | VFS/private metadata 合同不一致 | attribute operations 返回的 inode number/type 与待构造 identity 不符 | 构造触发内部合同失败 | 阻止把 foreign/corrupt state 发布到其他 identity | 2 | 发布前校验 immutable fields；后续 KVFS 与 core 直接访问同一组件，不执行可失败的 snapshot 回灌 |

## 故障管理

Core `Ext4Error` 通过 `into_vfs_err` 转换并返回 callback。Bridge 不回滚已由 journal 接受的
metadata，也不建立第二套事务；journal abort/recovery evidence 由 core 管理。`VfsInode::drop`
不能把错误返回给调用者，因此 final eviction failure 会记录 warning，后续 mount recovery
依赖遗留 orphan/recovery evidence 完成清理。

## 隐私分析

Bridge 传输文件数据、目录名、symlink 和 metadata，但不记录其内容。日志消息只包含 inode
number、操作阶段和错误，不应输出文件内容或 xattr value。

## 已知限制

- Mutation 仍受挂载级 write guard 串行化。
- 精准 per-inode sync/datasync tid 尚未进入 KVFS runtime inode，fsync 保守提交 running
  transaction。
- KVFS xattr operation trait 尚未接入，core xattr 能力未暴露为 live syscall surface。
- Final Drop 错误只能记录，不能同步返回给已经消失的最后引用持有者。

## 审计清单

- 新 callback 是否使用 `self.core_inode`，而不是按 number 重新 `iget`？
- 新 ext4-private state 是否放在 VFS inode 组合持有的 private state，而不是 mount map？
- 新 generic attribute 是否仍由同一 inode component 实现，而不是在 bridge 增加副本或回灌？
- unlink 是否延迟 data/xattr/inode slot 释放到 final VFS eviction？
- PageCache 与 core lock 的获取顺序是否保持无反转？
- delalloc interval/count 和 mount aggregate 是否只经 core range API 联合更新？
