# kext4_vfs — 设计文档

## 定位

`fs/bridges/kext4_vfs` 把 `kext4` 存储核心接入 KVFS。它拥有 VFS wrapper、
PageCache/address-space callbacks 和 open-file 生命周期，但不复制 generic inode attributes，
也不拥有 ext4 磁盘格式或独立的 decoded inode snapshot。

## 背景

KVFS 需要一个稳定的 `VfsInode`/`AddressSpace` identity；KExt4 只需要与 Linux
`ext4_inode_info` 对等的文件系统私有状态。二者不能各自建立 resident cache。bridge 必须在
KVFS 已保留 `I_NEW`-equivalent slot 后加载 ext4 状态，再把该状态组合进唯一的 `VfsInode`。

## 范围

```text
src/lib.rs    filesystem-type registration
src/fs.rs     mount state, superblock construction, sync and eviction hooks
src/inode.rs  inode/file/address-space callbacks
src/util.rs   KExt4/KVFS type and error conversion
```

## 架构

```text
KVFS-wide identity table
  -> (SuperBlock, ino) -> New | Live(Weak<VfsInode>) | Freeing
                                      |
                                      v
          VfsInode / AddressSpace / generic inode semantics
                                      |
                       InodeAttributeOperations
                                      |
                                      v
 kext4_vfs::Inode { Arc<bridge fs>, one Ext4Inode component,
                    writeback_lock }
```

KVFS-wide table 按 `(SuperBlock, ino)` 联合索引，是唯一 resident identity table。这对应
Linux `inode_hashtable`，不会给 `SuperBlock` 或 bridge filesystem 增加 cache 字段。每个 bridge
`Inode` 直接组合持有一个
`Ext4Inode`；KVFS 通过 `InodeAttributeOperations` 直接访问该组件中的 mode、owner、nlink、
timestamps、`i_size` 和 block accounting，因此没有第二份 cached attributes，也没有 mutation
后的 bridge 回灌。组件同时保存 `i_disksize`、extent root、xattr、delalloc 等 ext4-private
状态，但不拥有 resident lifecycle。read、writeback、truncate、link、unlink、rename、sync 和
final eviction 始终传递这个组件，不按 inode number 重载。

## 调用约束 / 执行上下文

bridge callback 运行在可阻塞的任务上下文，可能取得 sleepable lock、分配内存、访问 PageCache、
执行块 I/O、journal commit/checkpoint 或设备 flush，不可从中断上下文调用，也不适用于 block、
scheduler 和 allocator 尚未就绪的 early boot。它不依赖 CPU-local 状态，但依赖有效的 KVFS
superblock、当前 callback 所持有的 VFS inode 引用和已挂载的块设备。

## 状态机

```text
KVFS cache reserves New -> core decodes private state -> publish Live VfsInode
    -> namespace unlink sets nlink=0, objects remain resident
    -> last VfsInode Arc Drop marks cache entry Freeing
    -> SuperBlockOperations::evict_inode on the held core object
    -> PageCache/delalloc teardown
    -> core prepare -> batched extent release -> finish
    -> remove cache entry and wake iget waiters
```

`release()` 是每个 file description 的 close callback，只处理最后 writer 的 EOF preallocation
策略；它不是 final inode teardown。`New` 或 `Freeing` 状态下的同号 `iget` 在 KVFS 内等待并
重试，不能把过渡状态作为用户错误返回。

## 算法流程

Mount 先分配 nascent `SuperBlock`，再由 root initializer 使用
`SuperBlock::get_or_try_init_inode()` 读取 ext4 root；普通 lookup 从当前目录 inode 取得同一个
superblock 并进入同一 API。命中 `Live` 直接返回现有 `VfsInode`；命中 `New/Freeing` 等待；
只有获得空 slot 的 owner 才调用 core inode decode，构造 bridge private state，在绑定
superblock 后发布 `Live`。root 初始化失败时 VFS 删除已注册的 nascent superblock identity，
`New` inode slot 随对象一起销毁，等待同一设备的 mount 调用者被唤醒并允许重试。

本 crate 拥有唯一静态 `FILE_SYSTEM_TYPE`，并由自己的 `register_init` 回调将它注册进 KVFS。
`kruntime` 不依赖或列举 ext4 后端。其 `get_tree`
先进入 KVFS `get_tree_bdev()` 完成 source/device policy 和 `(s_type, dev_t)` identity
reservation；已有实例直接复用，只有新生 reservation 调用 `Ext4Filesystem::fill_super()`。
KVFS 在调用前已经给 nascent `SuperBlock` 建立 canonical `s_type/s_bdev/s_flags`；fill-super
只从该对象取得 `s_bdev`，安装 ext4 operations 与 root，不接收或复制 type、device、flags。
root boot 和用户 `mount(2)` 不存在另一条 ext4 mount callback 或实例缓存。

Buffered writeback 由每个 bridge inode 的 `writeback_lock` 串行化一个 PageCache writeback pass。
delayed-allocation 区间树、`i_reserved_data_blocks` 等价计数和 mount-wide aggregate 全部归
KExt4；bridge 只按逻辑区间调用 reserve/release/truncate。PageCache traversal 不持有 core
write guard，batch callback 在 folio/mapping lock 释放后短暂进入 core。
Set-length 在 backing prepare 前分别记录旧 `i_size` 与 `i_disksize`：`truncate_setsize()` 丢弃
folio 后，只要新长度小于旧 `i_size` 就释放 EOF 后的 delayed intervals；只有新长度小于旧
`i_disksize` 才继续释放磁盘 mappings。Core prepare 只发布 `i_disksize`，不得提前修改
`i_size`；否则 KVFS 会丢失 PageCache 的真实旧 EOF。两种判断不得合并。完整 `getattr` 只消费
core 在一次 inode-state 锁内生成的瞬时 `kstat` 对等快照，不逐字段重复加锁，也不保存该快照。

## 并发模型

Bridge filesystem 使用挂载级 `RwLock<kext4::Ext4Filesystem>`。读目录、lookup、read、extent
查询可共享 read guard；metadata mutation、allocation、journal 和 eviction 使用 write guard。
同 inode writeback 另由 `writeback_lock` 串行化。锁序不允许持有 PageCache mapping/folio lock
等待 core lock。KVFS cache mutex 只保护 identity state；等待发生在释放 mutex 之后，并使用
目标 cache slot 的等待队列，不由无关 inode 的状态变化唤醒。

## 设计决策

- VFS-wide `(SuperBlock, ino)` table 是唯一 resident cache，负责 `I_NEW`/`I_FREEING`
  等价等待、`VfsInode` 和 `AddressSpace` identity。
- KVFS generic attribute API 与 KExt4 共用同一组件；`i_size` 与 `i_disksize` 是两个不同的
  Linux 字段，不是两份同义 cache。
- Bridge `Inode` 不保存 inode number 字段；number 由其组合持有的 core private component 给出。
- `writeback_lock` 保留在 bridge，因为它保护 PageCache callback 编排，不是 ext4 inode metadata。
- KExt4 不建立 inode-number map；其 `Ext4Inode` 只作为 bridge inode 组合持有的 inode component。

## Drop / 资源释放

`VfsInode` 最后引用 drop 时先执行 generic PageCache final truncate，再释放该 inode 的 delayed
reservation。linked inode 只结束本次 clean resident wrapper；zero-link inode 对 bridge 持有的
private state 执行 journaled final eviction。KVFS 在回调前已把 cache entry 置为 `Freeing`，
回调结束后无论成功或失败都删除旧 entry 并唤醒等待者；失败的磁盘清理由 ext4 orphan/recovery
证据继续承接。
