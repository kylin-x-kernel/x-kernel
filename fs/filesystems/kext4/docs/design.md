# KExt4 — 设计文档

## Test Layout

Guest unit, component and recovery tests live under `ut-tests/`, mirroring the
owning source module. Shared fault devices and superblock fixtures live there
too. `unittest::path_mod_test!` retains private implementation access without
turning these kernel fixtures into Cargo host integration tests.

## 定位

`fs/filesystems/kext4` 是 X-Kernel 的 ext4 文件系统实现。它同时拥有 KVFS operation
对象和 ext4 私有算法，负责磁盘格式解析、元数据校验和、JBD2 恢复与事务、extent、
分配器、truncate/orphan、namespace mutation、xattr 以及 ordered-data writeback。
运行态不再存在 `kext4_vfs` bridge crate 或另一套 superblock/inode wrapper。

磁盘 superblock 的 compat、incompat 和 ro-compat feature 字段分别使用独立的
`bitflags` 类型表示。解码使用 `from_bits_retain` 保留未知磁盘位，避免把不同 feature
类别中数值相同的位混为一类，并保证挂载协商仍能报告原始 unsupported bits。

当前 `Ext4SbInfo` mount state 同时发布读取和 mutation API，类型系统尚未区分只读
legacy block-map mount。因此挂载协商要求 superblock 启用 `EXT4_FEATURE_INCOMPAT_EXTENTS`：
整盘 `^extents` 格式在读取 group descriptor、replay journal 或发布 VFS root 前返回
`Unsupported(NonExtentFilesystem)`。带 extents feature 的镜像仍可只读解析个别 legacy inode，
但其 buffered-write reservation、`page_mkwrite` 和后续 mutation 会在复制或标脏用户数据前返回
`Unsupported(NonExtentInode)`。完整放开整盘 legacy 格式必须同时实现
direct/single/double/triple indirect 的分配、释放、truncate 与 resize，不能只移除挂载检查。

## 背景

KExt4 是 X-Kernel 唯一的 ext4 后端，保持 Rust 受检代码、明确的 feature negotiation、
日志化元数据更新，以及 e2fsprogs/e2fsck 互操作能力。`KFEAT_FS_EXT4` 直接把 KExt4
链入运行态，不再经过 bridge 或实现选择层。N0 已补齐 KVFS 单一 resident inode identity、
cached attributes、两阶段 truncate 和 open-unlink/final-evict 基线。N1 已建立 persistent journal
和 mount/journal 生命周期边界，N2 继续建立 buffered 并发执行框架，再在 N3 集中补齐 crash
recovery、错误观察和 unmount/freeze。

## 范围

主要源文件：

```text
fs/filesystems/kext4/
├── src/lib.rs
├── src/vfs.rs
├── src/superblock.rs
├── src/journal.rs
├── src/jbd2/
├── src/buffer/
├── src/extent/
├── src/balloc.rs
├── src/ialloc.rs
├── src/mballoc.rs
├── src/prealloc_space.rs
├── src/file.rs
├── src/truncate.rs
├── src/orphan.rs
├── src/namei.rs
├── src/dir.rs
├── src/dirhash.rs
└── src/xattr.rs
```

## 架构

```text
KVFS syscall / PageCache
    |
    v
fs/filesystems/kext4
    |-- Ext4SuperOperations (static KVFS s_op, no mount identity)
    |-- Ext4AddressSpaceOperations (static KVFS a_ops, no inode identity)
    |-- RwLock<Ext4SbInfo> (the sole ext4 s_fs_info object)
    |-- layout / feature negotiation / device
    |-- HTree hash_unsigned mount policy
    |-- metadata buffer / checksum validation
    |-- group descriptors / allocator state
    |     |-- per-group free-extent cache
    |     |-- orphan 头与 RECOVER/ORPHAN_PRESENT 位
    |     `-- speculative prealloc space 两侧索引（inode 逻辑图 + group 孪生表）
    |-- MountedJournal (类似 journal_t)
    |     |-- internal journal mapping / on-disk superblock
    |     |-- transaction state
    |     |     `-- one object: Running -> Committing -> Checkpoint -> Finished
    |     `-- FIFO checkpoint queue / runtime head-tail state
    |-- delayed-allocation mount aggregate
    `-- extent / orphan / namei / xattr / truncate algorithms
           ^
           `-- borrow Ext4Inode private state composed in the VFS inode
    v
block::BlockDevice
```

核心修改路径通过 `JournalHandle` 进入事务，先经 buffer 层取得元数据创建/写入访问权，
再修改元数据字节和内存中的布局状态。`MountedJournal` 是生产路径唯一的 journal identity：
它拥有磁盘 superblock、ring 运行态和 transaction 状态。一个 transaction 对象依次经历
Running、Committing、Checkpoint 和 Finished phase；FIFO 队列只保存该对象本身，持久化证据
属于它的 Checkpoint phase，不复制 commit payload，也不保存指回另一 coordinator 的引用。
普通 mutation 不负责同步完成 home-block checkpoint。

## 整块文件读取与缓存提交

`Ext4SbInfo::read_inode_bytes` 按映射 run 读取文件。只有 initialized 映射、文件偏移位于
filesystem block 边界且本段长度为整块倍数时，才把调用者已有的目标切片交给
`FilesystemDevice::read_blocks`；省去该段临时零初始化 `Vec` 的分配和一次回拷。
目标切片可以从任意字节地址开始；`BlockDevice` 的同步读写将该范围直接交给 Bio，
由后端与既有 DMA 能力层解决设备地址对齐、连续性、映射及缓存维护要求。
同步调用在设备访问结束后才返回，通用层不复制完整块缓冲区。
ext4 不保存设备对齐常量或驱动清单，也不为设备地址要求额外创建回退缓冲。
`FilesystemDevice` 负责 filesystem/device block 换算和长度、范围校验。
部分块仍需 staging 来提取文件范围，与设备地址要求无关；驱动的 DMA/bounce buffer 所有权
协议不变，这不是端到端零拷贝。

部分首尾块继续用 staging buffer；hole 和 unwritten extent 仍只填零，返回长度仍受 inode
EOF 限制。`read_at` 返回错误时目标缓冲可能已被后端修改，不能把其前缀当作成功读取。
KVFS/PageCache 单页填充只有在 `read_folio` 成功后才能置 uptodate；ext4 批量 readahead
只有在整段 backing read 成功后才调用 `complete_folio`。失败后的需求重试必须完整初始化
目标页，才可供 buffered read 或文件映射使用。

`file/read_tests.rs` 使用 1/2/4 KiB filesystem block 与 512 B/4 KiB device block 组合，覆盖
对齐直读、非对齐切片回退、短缓冲、hole、unwritten 和非零 17 字节 EOF 尾块；mock 同样拒绝
非 4 字节对齐的底层传输，但置于生产块适配器之后；非对齐调用仍须成功且内容正确，
设备中转不得把失败块的暂存内容复制回调用者。`vfs/read_tests.rs` 让真实
ext4 backing read 修改 17 字节前缀后持续返回 I/O 错误，验证单页保持非 uptodate、批量
预读不发布前缀、buffered read 不复制失败内容，以及完整重试后的数据和 EOF 补零。可写 journal
fixture 另验证失败读后的部分 buffered write：读错误持续时不能推进写入；成功重试必须先完整
读页，随后只修改指定字节，读回保留其余数据，不能把失败前缀变成有效的 dirty 数据。

## 调用约束 / 执行上下文

KExt4 核心 API 可能执行块设备 I/O、内存分配、JBD2 事务、checkpoint 和设备 flush，
因此属于任务上下文 API，允许阻塞，不适合在中断上下文调用。它也不适合在块设备、
分配器和 journal 状态尚未可用的早期启动阶段调用。

ext4 的 canonical `FileSystemType` 由本 crate 的 initcall 注册。`get_tree_bdev` 先按该静态
类型对象和 canonical `BlockDevice` 执行 Linux `sget_dev()` 语义、取得独占 device claim 并
分配 nascent KVFS `SuperBlock`；`fill_super` 只填充这个既有对象，不接收或复制 `s_type`、
device 或 flags identity。已有同 identity 实例由 VFS 复用，失败填充由 VFS 释放 claim 并
唤醒等待者。

KVFS `SuperBlock` 分别保存无状态 `Ext4SuperOperations` 与唯一
`RwLock<Ext4SbInfo>` private object，对应 Linux 分离的 `s_op`/`s_fs_info`；operation table
是所有 ext4 挂载共享的真正 Rust `static`，不是第二个 filesystem object，也不在挂载时分配
`Arc`。private object 由 superblock 独占，`private::<RwLock<Ext4SbInfo>>()` 只返回绑定于
superblock borrow 的引用。只读调用可共享进入，仍会联合修改 superblock/group metadata
的 mutation 由 write guard 串行化。delalloc mount aggregate 已由 `Ext4SbInfo` 内部独立 mutex
保护，reserve/release/truncate reservation 只取得 shared mount guard 与该真实计数锁，不再取得
mount write guard。该挂载级锁不保护 KVFS inode cache、AddressSpace 或 open-file 生命周期。
VFS inode 组合持有的 ext4 private state 以 per-inode
`RwLock<Ext4InodeState>` 保护磁盘 metadata working state 和 delayed-allocation extents：
只读路径（`size`/`stat`/`metadata_snapshot`/delalloc 查询）通过 `with_state` 以 read guard
共享进入，mutation 通过 `update_state` 以 write guard 独占；resident lifecycle 完全归 KVFS。
调用者不能假设 allocator 或 journal
已有 per-group 级别的并行修改。可写文件打开计数由
KVFS `VfsInode` 拥有，kext4 只在 release callback 中读取它。

所有 regular-file/symlink mapping 共享同一个静态 `Ext4AddressSpaceOperations`。aops callback
从 `mapping->host` 取得唯一 `VfsInode`，再借用其中的 `Ext4Inode` private component；
`Ext4Inode` 不再同时充当 aops object，因此不存在两个必须保持一致的 inode identity。

allocator 可变状态（逐组可变 descriptor 字段、per-group free-extent cache、
free-blocks/free-inodes/directory-inode 全局计数、orphan 头、`RECOVER` 与
`ORPHAN_PRESENT` 位）由 `Ext4SbInfo` 内的
`Mutex<AllocatorState>` 保护；mount-wide delayed-allocation 总量刻意放在该锁之外，
由独立的 `delalloc_reserved_blocks: Mutex<u64>` 保护，对应 Linux
`s_dirtyclusters_counter`（per-CPU 原子计数）的独立更新域——reserve/release 只与
其他 delalloc 修改互相串行，不阻塞全局组分配。静态几何不受本锁变动：
primary superblock 解出的 geometry/static feature 留在挂载时冻结的 `superblock` mirror，
逐组地址（block/inode bitmap、inode table）只保存在挂载时冻结的 `group_geometry`
一维表中——`AllocatorState` 的逐组条目（`GroupMutableState`）只镜像计数、flags 与
bitmap checksum 等可变字段，刻意不含地址字段，地址因此只有一个读取源，对应 Linux
`ext4_get_group_desc(..., NULL)` 对静态地址字段的无锁读，inode 表定位等热路径据此
无锁读。journal replay 可能重写 descriptor block，`reload_mutable_metadata_state`
重解码并校验 replay 后的 descriptor 表：计数等可变字段照常重建，地址字段与冻结表
不一致时 fail-closed 拒绝（KExt4 支持的操作不会 journal 地址变更，变化即损坏或不
支持特性），避免冻结几何与重载状态分裂后操作错误的 bitmap/inode table。取锁前，
balloc/ialloc 入口先完成冻结几何解析与元数据缓存预热（`prefetch_metadata_blocks`），
冷缓存设备读在锁外完成、失败也在锁外返回，不串行化全体分配器（对应 Linux 先读
bitmap buffer 再 `ext4_lock_group`）；权威快照在锁内重读，预热结果绝不作为权威
数据。balloc/ialloc 的
单组分配/释放入口（`*_in_group`）整体为单一临界区：从读组描述符、位图 buffer 读改、
journal write-access 到内存状态发布全程持锁，入口之间不会基于同一位图快照重复选取
同一块/同一 inode，也不会丢失计数更新——对应 Linux 组锁"锁窗覆盖分配决策与
bitmap 更新"的语义。多组扫描入口（`allocate_blocks_for_write()` /
`allocate_inode_with_name()`）不是单一临界区：扫描循环逐组以短锁窥探 hint、随即
释放，再逐个在 `*_in_group` 中重新取锁尝试；由于该锁不可重入，不能在扫描循环内持锁
调用 `*_in_group`，goal 选择与分配也不构成原子操作，组间竞态由 `*_in_group` 锁内
重校验兜底。delalloc 总量更新位于独立小锁的单临界区，读改写不会丢计数；预算准入
检查与累计位于同一临界区（allocator 锁读 free-blocks + delalloc 锁读并累加，
锁序固定 allocator → delalloc），check-then-act 基于同一快照原子完成，不存在
并发超预留窗口，也不依赖 bridge 挂载级写锁的外部串行化；临界区有界（仅读计数
并做一次加法），与 Linux 直接原子累加 percpu 计数的近似记账相比保持精确语义。

锁序固定为 bridge 挂载级锁 → allocator 锁 → delalloc 锁 / prealloc map 叶锁 → metadata-buffer /
journal 内部锁，禁止反向等待。delalloc 总量锁与 per-inode prealloc map 叶锁都是 allocator 锁之下的
独立叶锁，彼此不嵌套：前者只保护 mount-wide 计数，后者只保护单个文件的逻辑区间表，`update` 闭包在
allocator 临界区内取放该叶锁且不取得任何其他锁；group 侧 discard 需要锁 foreign owner 的 map 时按
同一顺序取，故 prealloc 操作不与 inode-state 锁构成 ABBA。持 allocator 锁期间不得调用任何会重新取该锁的路径：
不仅公开入口，取锁访问器（`groups()`/`free_blocks_count()`/`free_inodes_count()`/
`needs_recovery()`/`has_orphan_present()`）同样在禁止之列——守卫内直接访问锁内字段；ENOSPC 重试阀
（`discard_preallocs_for_space()`）同样在此禁止之列，它自带 allocator 锁，因此只能留在扫描循环的
锁段之外；
inode 定位等几何
计算读冻结的 `group_geometry` 表（无锁），持锁的分配路径使用 `*_in_group` 无锁
变体（该锁不可重入，ksync 对同任务重入直接 panic）。delalloc 锁是独立的
`Mutex<u64>`，delalloc 路径只对它加锁，读 free-blocks 时按上述锁序先取 allocator
短锁。对齐目标仍是 Linux 的 per-group spinlock + percpu 计数：当前单锁是过渡形态，
临界区有界（单组位图扫描 + 已缓存元数据的内存读改写 + journal 发布；阻塞设备读
已经预热移出锁外，仅剩地址依赖分配结果的 inode-table 块仍在锁内读），后续按组细化，使不同
组的分配恢复并行（Linux `ext4_lock_group` 锁窗 = 单组扫描，不跨组持锁）。

该挂载级读写锁是过渡实现而不是长期调用契约。N1 已把磁盘 journal、transaction engine 和
checkpoint queue 收进同一个 `MountedJournal` 生命周期边界；metadata、allocator、device 和
geometry 继续由 `Ext4SbInfo` mount state 持有，对应 Linux `ext4_sb_info` 的聚合角色。
N2 根据真实执行者建立 journal、per-inode、metadata-buffer 和 per-group 锁，替代 mount-wide
全局串行化。N2 per-group 锁验收标准：锁窗内不得含阻塞 I/O——位图/描述符锁外预取
（对齐 Linux 先读 bitmap buffer 再 `ext4_lock_group`），脏元数据发布移到锁外完成
（对齐 Linux `ext4_unlock_group` 之后才 `ext4_handle_dirty_metadata`），锁窗只保留
纯内存位操作与计数更新；发布外移必须配套锁内重校验与失败重试，防止并发观察到过期
位图。在 N2 完成前，禁止向分配器锁临界区新增阻塞 I/O 或新的取锁调用。
所有这些路径仍属于可阻塞的任务上下文，不能从中断上下文调用。

Mount 首先把 primary superblock 解码为经过结构和 metadata checksum 验证的磁盘状态，再由
`Ext4SbInfo::open()` 执行 feature negotiation；non-extent 等运行能力拒绝发生在 layout、
journal recovery 和 VFS root 发布之前，但不得抢在 checksum failure 之前改变错误语义。
Recovery replay 后重新加载 mutable superblock 时会再次执行同一 mount capability 校验。
只更新 superblock 计数、recovery bit 或 orphan head 的磁盘 helper 复用格式 decoder，不承载
mount policy。

## 状态机

### VFS resident inode 与 ext4 private state

```text
KVFS SuperBlock::get_or_try_init_inode(ino)
  -> absent: reserve New, decode one Ext4Inode private state, publish Live VfsInode
  -> New: wait for initialization
  -> Live: return the existing VfsInode
  -> Freeing: wait for final eviction, then retry

absent -> New -> Live -> Freeing -> absent
```

`VfsInode`/`AddressSpace` 是唯一 resident identity，状态层次对应 Linux `struct inode` 的
`I_NEW`、普通可用状态和 `I_FREEING`。KVFS-wide `(SuperBlock, ino)` table 先占据 `New`
slot，再让唯一 initializer
解码 `RawInode` 并构造唯一的 `Ext4Inode` private component；初始化失败会撤销 slot 并唤醒等待者。最后一个
VFS 引用进入 drop 时，KVFS 在调用 filesystem eviction hook 前发布 `Freeing`；同号 lookup
等待 cleanup 完成并重新查找，不把竞争暴露为 `EINVAL` 或 `ESTALE`。

`RawInode` 只是经校验的磁盘表示。`Ext4Inode` 对应组合在 Linux `struct inode` 内的
`ext4_inode_info` 私有部分：它没有 inode-number cache、resident lifecycle 或 eviction
引用计数。普通 lookup 只允许 KVFS `New` slot 的 owner 从磁盘构造这份状态；unlink、rmdir、
link 和 rename 必须传入 VFS 已持有的 child/moved/replaced private state，core 不按编号重新
解码 live inode。Recovery 在尚未发布 mount/VFS identity 时可以构造临时状态处理 orphan。

Metadata mutation 先 stage/publish journal buffer，再在同一 inode component 中发布结果。
该组件同时承载 KVFS 通过 `InodeAttributeOperations` 访问的 generic fields，以及
`i_disksize`、extent root、ext4 flags、xattr block 等 ext4-private fields；这对应 Linux
`ext4_inode_info` 内嵌 `struct inode`，不是两份 attribute cache。`i_size` 可以在 ordered
writeback 前领先 `i_disksize`，两者是 Linux 本来就有的不同字段。kext4 KVFS 层不再执行 mutation
后的 metadata snapshot 回灌，也不创建备用 resident wrapper 或按 inode number 重载 live state。
Regular-file metadata publish 只提交 `i_disksize` 等 ext4 状态，不能因为旧 `i_size ==
i_disksize` 就顺带修改 `i_size`；后者只由 VFS `write_end_set_size()` 或
`truncate_setsize()` 在 PageCache 顺序点发布。目录、符号链接等由 ext4 算法直接改变长度的
对象则在对应 metadata mutation 中显式发布其可见长度。

`i_size` 是通用 inode 属性，`i_disksize`、extent root 和 extent-status proof 是 ext4
私有映射状态；组合在同一对象中不表示它们的单值读取必须使用同一把锁。唯一的 `i_size`
保存在 `Ext4Inode::visible_size: AtomicU64`，从 `Ext4InodeState` 移出而非复制；
`InodeAttributeOperations` 直接访问这一字段，不创建额外属性对象或影子缓存。
全部受支持架构均为 64 位，对应 Linux `include/linux/fs.h` 的 `i_size_read()`
acquire load 与 `i_size_write()` release store：缓存命中读取 EOF 不取得 ext4
inode-state 锁；看到一次尺寸发布的读者也能看到该发布之前的相关修改。这个单值读取
不保证之后长度不变，也不固定块映射。

尺寸发布继续持 inode-state 写锁，使 shrink 的 extent-proof 清除与新 EOF 对映射读者
不可分割；`sizes()`、`stat()` 和 extent-status overwrite proof 在该锁内采样尺寸，
排除尺寸和映射状态的并发发布。`sizes()` 只保证采样期间没有这两类写者，并不把
`i_size`、`i_disksize` 的不同发布阶段合并成事务；两者在 writeback/truncate 期间可以不同。
这把锁是当前 ext4 状态的同步实现，不能让通用 `i_size` 单值读取依赖它。

上层 inode data 锁对应 Linux `i_rwsem` 的尺寸事务串行化；truncate 另持 mapping
invalidation 排他锁。`write_end_set_size()` 在页内容准备完成后发布增长，
`truncate_setsize()` 按发布尺寸、PTE unmap、PageCache resize、再次 unmap 的次序执行，
随后才释放 backing blocks。Atomic 只提供单值发布与内存可见性，不提供这些事务或失效互斥，
也不改变 regular-file backing metadata/writeback 只发布 `i_disksize` 的职责。

### 元数据事务

```text
加入或创建 running transaction，并预留 credits
  -> 取得元数据创建/写入访问权
  -> 修改元数据字节和内存计数器
  -> 完成 ordered-data dependency
  -> 根据 credits、age、space 或 explicit sync 冻结 transaction
  -> 首次写日志前把 clean journal 激活为可恢复状态
  -> 持久化 journal commit
  -> 独立推进 checkpoint / journal tail
```

含义：

1. Handle 加入 mount-wide running transaction，并根据 mutation 类型预留 journal credits；
   transaction engine 持有挂载时确定的 credit limit，`begin()` 和运行中扩展都以同一上限校验，
   调用者不能传入另一个上限绕过约束。
2. 每个被修改的元数据 block 通过 buffer 层记录撤销/写入访问权。
3. 元数据字节和内存计数器在同一事务内更新。
4. ordered data 在使其可达的 metadata commit 前完成。
5. Commit 只冻结并持久化对应 transaction，随后允许新的 running transaction。
6. Checkpoint 独立写 home blocks；`fsync`、`syncfs`、unmount 和 journal-space pressure 按
   各自 durability intent 等待相关状态。

当前实现已由 mount 持有唯一 `MountedJournal`，journal sequence、单一 transaction phase
状态和 checkpoint 完成水位不再随每次 mutation 重建。磁盘日志能从运行态 append head
连续追加多个 committed transaction：活跃 journal superblock 只持久化指向最老未 checkpoint
transaction 的 sequence/start；`s_head` 是 clean/unmount 信息，不被当作活跃期运行态 head。
下一次追加位置由最近一次持久化 commit 以及 mount 内存状态确定。FIFO checkpoint 只推进 tail，
直到最后一个 transaction 完成才清零 start、写入 clean head 并清除 ext4 `needs_recovery`。
环形空间计算始终保留一个空 block，避免 head 追上 tail 后覆盖仍可 replay 的 descriptor。
clean journal 的首次 commit 会先持久化并 flush 非零 `s_start`，再写 descriptor/data/commit；
因此在激活和 commit block 之间掉电时，恢复会把日志识别为 active 并忽略未完成 transaction，
而不会错误地按空 journal 跳过扫描。

同步 commit 会把 descriptor、data、revoke 和 commit block 聚合为不超过 128 KiB 的有界
write batch；batch 在 journal ring wrap 和 internal-journal 不连续 physical extent 处拆分，
不会为了减少请求而跨越非连续磁盘映射。挂载时已校验的完整 journal-superblock block image
由 `JournalSuperblock` 缓存，后续 sequence/start/feature 更新基于该 image 生成并重新解码，
不在 commit 热路径重复读取 journal block 0。clean-journal activation flush 和 transaction
最终 durability flush 仍是两个独立边界；`sync_inode` 只在 commit 已经完成最终 flush 时省略
紧随其后的重复设备 flush，没有 metadata transaction 的 mapped-data overwrite 仍会显式 flush。

设置 ext4 recovery feature 时只更新磁盘 recovery evidence 和内存 feature 状态，不再用尚未
checkpoint 的旧 home-block superblock 覆盖较新的内存 allocator counters。真实 Linux ext4
镜像回归覆盖 committed transaction 扫描、最终 clean 和 e2fsck；组件测试进一步验证
共享 metadata 的两个事务批量 checkpoint 后保留最新版本，并合并 home/tail 屏障。
若 transaction 包含 primary superblock，persist 路径会把 recovery feature 同时合并进 journal
记录和该 transaction 的 frozen checkpoint image；因此较老 checkpoint 在后续 commit 仍 pending
时不会把磁盘 `needs_recovery` 错误清零。只有 journal tail 真正清空后才单独清除该标志。

普通 mutation 在 handle 内决定成功或失败，并可与后续 mutation 共享同一个 running
transaction。新 handle 加入前按 journal 格式开销采用约三分之一日志容量作为普通 transaction
上界；handle stop 会归还未使用 credits，因此 outstanding credits 表达仍在事务中占用的真实
容量。不再以固定 operation 数或“半个 journal”触发普通提交。home-block checkpoint
仍留在 FIFO queue；`syncfs` 和 KVFS unmount writeback 会先提交当前 running transaction，
再 drain 全部 pending checkpoint；普通 mount 在 dentry eviction 后再次执行同一同步路径，
以覆盖 final inode eviction 产生的新 metadata。正常提交直接检查 running/committing 槽位，
只有针对旧 pending transaction 的重试才扫描 FIFO，避免每次提交都承担 backlog 长度的开销。

checkpoint worker 在 mount 写锁下执行两类工作：显式同步快照全部队列并排空；append
返回 `JournalBusy` 时，选取达到空间目标的最短连续前缀。令 `A` 为当前 frozen transaction
实际需要的 descriptor、metadata、revoke 和 commit 块数，`B` 为 mounted transaction credit
上限对应的最坏日志开销，`C` 为可用 ring 总块数，则回收目标为 `min(C - 1, A + B)` 个可写
日志块；物理 free space 必须严格大于该目标，继续保留一个空块。每个 credit 最多拥有一个
metadata block 或 revoke record；若 descriptor 容纳 `D` 个 tag，credit 上限为 `L`，则
`B = L + ceil(L / D) + 1`。若 metadata 块数为 `m`、revoke 记录数为 `r`，revoke 每块
容量为 `R`，则 credit 计费保证 `m + r <= L`，格式容量校验保证 `D >= 1`、`R >= 1`。
因此实际编码开销满足：

```text
m + ceil(m / D) + ceil(r / R) + 1
    <= m + r + ceil(m / D) + 1
    <= L + ceil(L / D) + 1 = B
```

这个上界不要求 `R` 与 `D` 具有大小关系。混合记录的 descriptor/revoke 控制块数可能比
全 metadata 的 descriptor 数多，但把一个 metadata credit 换成 revoke 至少省去一个
完整 metadata 数据块，足以支付最多一个 revoke 日志块；末尾 `+1` 是 commit record。
因此预算覆盖任意合法混合，无须额外增加 revoke 份额。小 ring 可通过 `C - 1` 上限削减
下一笔的可选预算，但不能削减当前 append 的必需空间；截断后不保证下一笔免于再次回收。
`usable_log_capacity()` 统一定义 ring 容量减去一个预留空块，append 准入、单笔事务
过大检查和 checkpoint 目标封顶均使用该值，确保全排空后目标始终可达。

这是空间不足后的回收批次目标，不是 handle 准入时的日志空间预留协议。
`begin` / `reserve_more` 当前检查事务 credits，未发生 `JournalBusy` 的 append 不建立
下一笔预留；commit 仍可能在 metadata 已修改后推动同步 checkpoint。将空间保证纳入
事务准入需要同时覆盖 handle 扩展、running/committing 额度及释放生命周期，独立于本策略。

压力选择只读取选中前缀、首笔后缀和队尾凭据，不先复制整个队列。选中事务按顺序写回
frozen metadata（同一块的较新版本最后写入），统一执行一次 home-block flush，再一次更新
并 flush journal tail。只有这两道持久化屏障及适用的 recovery 状态处理成功后，才逐个释放
事务及其日志空间；写回、flush 或尾部更新失败会保留选中和未选中的全部 pending work 并
abort journal。保留后缀时先按退休事务涉及的 block key 定向回收 clean、无 pin、无新 owner 的
metadata cache entry。checkpoint 只负责释放退休事务的占用和定向回收，不用批次计数
驱动一般缓存清理。显式 `sync_filesystem` 在 drain 和最终 flush 后保留全缓存回收，
checkpoint worker 自身只定向处理退休事务，不因选中了全部 FIFO 而扫描无关 cache。
恢复标志落盘要求的缓存失效继续遵循原有一致性协议。

普通缓存的需求由 `MetadataBlockCache` 自身计算：当前驻留 block payload 超过 4 MiB
软保留额度时，需要推进回收；不维护 checkpoint 批次或预算镜像。
缓存另有一个 `has_reclaim_demand: AtomicBool` 提示，用于让没有回收需求的 mount
访问跳过 cache mutex。所有索引插入、删除和失效在已有 cache mutex 内根据实际长度
更新提示；不在普通 cache 命中或 mount 访问上递增计数。提示只使用 Relaxed 读写，
不发布 buffer 内容、不替代索引或淘汰资格的锁保护；观察到并发变化前的提示可能推迟
当前维护或触发多余的加锁复核，不影响缓存一致性。
`Ext4Mount` 是 KVFS superblock 的唯一挂载私有对象，内部仍用 `RwLock<Ext4SbInfo>`
保护 core；读写访问 guard 在结束时、释放 mount 保护之前，向 cache 提供最多检查 64
条目的扫描预算。只读 mount 访问同样提供入口，不需要先发生日志压力或 syncfs。

cache mutex 统一保护索引、需求复核、游标和淘汰资格；预算按检查条目计费，dirty、pin、
后续 ownership 和 frozen image 都保留。游标每轮固定结束 block key，避免持续新增高地址
推迟回绕。索引条目另带一个由同一 mutex 保护的 `was_referenced` 位：首次插入不算复用，
后续命中在已有索引临界区内置位，不增加锁或原子读改写。增量扫描遇到置位条目时清位，
本次保留并继续推进游标，仍计入 64 项检查预算；之后没有命中的条目才按原有资格淘汰。
这是一位 second-chance 策略，不维护精确 LRU 链表，也不在单次调用内绕回重扫来凑淘汰数。
热点混合冷访问及超容量循环访问的请求计数见
[元数据缓存回收验证](metadata-cache-reclaim-validation.md)。
checkpoint 定向回收和显式全量回收不受访问位延迟，仍检查 pin/ownership/frozen image。
一次访问只做有界扫描，不周期性全扫；剩余需求由后续 mount 访问继续推进。
普通 read/insert 自身不淘汰。allocator mutation 持有 mount 写 guard，回收在它的 allocator
锁及预取/权威重读窗口结束后执行；读 guard 的回收仍排斥 allocator mutation。回收不做
设备 I/O，因此不把新的冷读插入 allocator 临界区。显式全回收和失效重置扫描游标。

4 MiB 仅为 resident block payload 的保留策略，不统计 frozen image 和索引开销，不是
硬内存上限或实测最优值；64 仅限制单次访问的检查工作量，不保证成功淘汰数。全部条目
被 pin/事务持有、单次访问大量新增条目或新增速度持续超过扫描速度时，仍可超过额度；
被持续复用的条目也可能反复获得 second chance，额度不是强制驱逐热点的目标；
没有后续访问时也不主动运行回收。本实现没有通用 shrinker、容量准入或后台线程，
但不再用日志 checkpoint 节奏代替普通 cache 的回收需求。
批大小由空间目标和 FIFO 顺序决定，不跨越未提交的 running/committing transaction；
revoke 仍通过原有缓存规则抑制旧版本写回。提交者完成前缀回收后重试 append，后续事务继续留在 FIFO。

这在“仅够本次 append”的频繁屏障和“全部排空”的长时间同步等待之间保留下一笔事务的
空间预算，没有新增固定比例水位、后台 worker 或 age trigger。大事务或很小的 ring 仍可能
需要排空；mount 写锁仍覆盖设备 I/O，不提供单次提交延迟上界。吞吐和尾延迟是否改善须与
最短前缀及全量排空基线分别复测，预算本身不构成性能收益证明。

只有批次排空 FIFO 时，才按批末事务策略处理 recovery 状态；保留后缀时用 `next_oldest`
把日志尾部推进到剩余首笔事务，保持 journal active 和 ext4 recovery evidence，不执行缓存
失效或 mutable metadata 重载。`PreserveDuringRecovery` 事务追加后在 mount
写锁下立即同步 checkpoint，因此它若出现在批中必为最后一笔，不会有后续普通事务
在排空前加入。批中可以有更早的 `ClearAfterCheckpoint` 事务，整批仍保留 recovery
evidence 并重载最新 mutable metadata state。所有 policy 记录在 frozen image 写回前
校验存在。尾部校验借用现有 batch 中的 persisted evidence 迭代器，不构造第二份凭据容器；
全部连续性校验完成后才写入 journal superblock。这里仍逐事务写回 frozen image，尚未实现
跨事务 metadata block 写回合并。

精准 `fsync`/`fdatasync` transaction id 的所有权不在 journal 的 inode-number 全局表，而应在
VFS runtime inode identity 上，对应 Linux inode 内的 sync/datasync tid。现有 KVFS inode 尚未
提供该运行态字段以及“mutation 完成后发布 tid”的接口，因此 KExt4 当前采用保守语义：kext4 KVFS 层
先回写目标 inode 的 PageCache，core 再提交当时的整个 running transaction 并 flush 设备。
这保证 durability，但可能连带提交无关 inode 的 metadata。待共享 VFS runtime inode 接口具备
后，再实现目标 transaction 等待；不能重新引入按 inode number 索引的 mount-wide cursor map。
`syncfs` 和 KVFS unmount writeback 仍提交 running transaction 并 drain 全部 checkpoint。
当前 ordered-data dependency 记录在 running transaction 的 `has_unflushed_ordered_data` 中：
发布 metadata 的 writeback batch 在成功的数据 write 后设置该位，多个 PageCache batch 和 inode 可以共享同一事务。普通 batch
结束只更新内存 metadata，不单独 flush；freeze 把依赖位带入 committing phase，persist 在
任何 recovery evidence 或 journal write 之前统一 flush，成功后才清除依赖并继续提交。
因此 writeback 完成不代表当前事务的数据已持久化；inode/filesystem sync 或容量压力提交提供
持久化边界。metadata-only transaction 不增加数据 flush。仅覆盖已映射且不超过 `i_disksize`
的数据在 data-only 路径不加入 metadata 事务；inode sync 提交已有事务时的同设备 journal flush，
或无事务时的直接设备 flush，以及 filesystem sync 的最终 flush，保证同步返回时的数据持久化。
该路径不承诺覆盖内容先于无关事务的 commit record 持久化。`flush_ordered_data` 把设备 barrier
与依赖清除封装在同一入口：成功才清除，失败保留；barrier 期间不持有 journal/transaction 内部锁，
调用者仍须持有 mount 写锁串行化数据写入与 commit/checkpoint。
PageCache 仍以固定上限聚合 folio；dirty range 的必需数据块已由 `write_begin` 或
`page_mkwrite` 纳入 delayed-allocation accounting，writeback 不再扫描完整范围生成静态空间/
credit plan。实际写回由 byte cursor 推进，每处理一个 mapped、unwritten 或 hole allocation run
都重新调用 `map_blocks()`；unwritten conversion 和 hole allocation/insert/conversion 按实际
发生的 run 增长 credit usage，并优先通过 `JournalHandle::reserve_more()` 扩展当前 transaction。
transaction credit limit 只由 journal engine 持有并在 `begin()`/`reserve_more()` 内执行约束，
file 层不读取或重复解释该上限。若 handle 无法继续扩展，core 先 flush 当前数据、只推进该前缀
的 `i_disksize`，清除本事务已满足的数据依赖，同步提交 transaction 并精确释放该前缀的
delalloc reservation，再从同一 cursor 开启下一 transaction。optional preallocation 只使用扣除 reserved blocks 和全部
delalloc reservation 后的可用块，并保存在 cursor 中按 allocator 实际多分配的 block 扣减；
最后一个 transaction 才发布完整 timestamp/size。异步 dependency 对象和后台 writeback 属于
后续阶段。

operation savepoint、operation token 和 operation-local metadata byte copy 已删除。ext4
mutation 在首次 metadata access 前完成格式、目标状态、空间、credits 和 extent path
可表达性检查；allocator 在私有 bitmap/descriptor/superblock bytes 与 free-extent cache 副本上
完成计算后再发布。JBD2 handle 维护 credits 和 metadata/revoke membership，并向事务记录
ordered-data 依赖；显式 stop
归还未使用 credits 并返回 accounting 错误；journal 自身维护 abort 状态。多个 handle 可独立
stop。仅 metadata/revoke access 标记当前 handle 已发布更新，即使对应 block 已由
同一 running transaction 的前序 handle 加入；transaction membership 一旦发布，就不能由某个 handle
按路径局部删除，因为其他 handle 可能已共享同一 metadata block；后续 metadata access 失败会
abort journal。数据 I/O 的持久化依赖独立于该标记：metadata access 前的普通错误保留数据依赖，
允许脏数据重试，不因数据写成功而 abort。设备/checksum/状态机错误，以及任何发生在 metadata 已发布后的普通错误，同样
会永久 abort；commit 或 checkpoint I/O 失败也在返回错误前记录 abort，后续 sync 和 mutation
不能把残留的 `committing`/checkpoint state 当作成功。内存中已经发布的 bytes、buffer
ownership 和其他已成功 operation 的修改不会
跨 syscall 回滚。尚未发布的私有副本或刚取得但未发布的 ext4 资源仍由具体算法显式清理。
这与 Linux JBD2 一致：handle 负责 credits 和 buffer membership，失败通过 journal abort
传播，而不是建立第二套 syscall 事务系统；崩溃后一致性由磁盘 recovery evidence 和 replay
保证。

Linux 创建的 clean v2 journal 若尚未声明 revoke feature，首个 mutation 会先持久化开启该
feature，再允许 transaction/checkpoint 重叠；v1 journal 无 feature bitmap，继续退化为每次
commit 后同步 checkpoint。这样 metadata block 释放/复用仍满足下面的 revoke/reuse 约束。
普通 mutation 不再无条件 force-commit；成功 handle 关闭后修改留在 running transaction，
credits、journal space 和 explicit filesystem sync 决定同步阶段的 commit 时机。
truncate 的 orphan + `i_disksize` 更新仍强制 commit，保证释放旧 block mapping 前已有持久化
恢复点；recovery-time orphan cleanup 仍同步完成 commit/checkpoint。基于时间的 trigger 与后台
worker 留到同步驱动状态机稳定之后。

不带 JBD2 revoke feature 且无法升级的 journal 仍以同步 drain 作为安全前提：释放 extent/xattr
metadata block 时，core 从当前 handle 的 metadata 集合中 forget 已淘汰的 block，而不生成
磁盘不支持的 revoke record。因为新 mutation 开始前不存在更老的未 checkpoint transaction，
recovery 没有旧 metadata image 需要抑制。可升级的 v2 journal 则在任何 transaction 重叠前
flush revoke feature，后续释放路径必须写 revoke record。

`ExtentPath` 保存从 inline root 到目标叶子的各层 buffer、选中 entry 和逻辑上下界，并负责
叶子重写、索引 key 传播、均衡 split 与空叶 prune。路径查找把每层 bytes 直接移入路径，避免
为 parent sidecar 再复制一次完整 metadata block。常规 extent 插入和 unwritten 转换必须在
同一条路径内完成，不再进入全树重写；范围删除只在完整范围属于同一叶子时局部更新，跨叶
truncate/remove 会在任何 metadata 写入前回退全树重写。局部 split 会均衡新旧节点，避免
`capacity + 1` 条目形成“满节点 + 单条节点”并在后续插入时反复分配 metadata block。

### Namespace zero-link 删除

```text
namespace transaction
删除 dirent
  -> 降低 nlink
  -> nlink == 0 时优先加入 orphan-file slot，满时回退 legacy orphan list，并持久化 zero-link metadata
  -> 返回更新后的 parent/target inode 给 kext4 KVFS 层

最后一个 VFS inode/open-file 引用消失
  -> KVFS SuperBlockOperations::evict_inode
  -> 丢弃 PageCache 和 delalloc reservation
  -> 如果存在 external xattr block，先释放它
  -> truncate extent-backed data blocks
  -> 移除 orphan entry
  -> 释放 inode bitmap entry
```

Namespace transaction 不释放 inode number、xattr 或 data block。已有 VFS/open-file 引用继续
持有同一个 `VfsInode` 及其组合的 ext4 private state，所以 zero-link 后仍可读写；新的
namespace `iget()` 拒绝 zero-link inode，不会把 orphan 重新实例化为可达文件。对应的
namespace credits 只覆盖 dirent、nlink 和 orphan metadata；不能把后续 final eviction 的
extent、external xattr 和 inode bitmap 工作提前计入 rename/unlink/rmdir reservation。旧的
单事务 recovery/测试 eviction 路径按当前 extent tree 的实际 metadata targets 估算，运行态
kext4 KVFS 层则继续使用有界的三阶段 eviction。

### 写回 speculative prealloc space

```text
写回窗口分配 surplus      -> Registered（inode 逻辑图 + group 孪生表，同一 allocator 临界区发布）
后续写命中已登记区间      -> Consumed（整段/前缀/后缀/内部消费，剩余拆为新的 Registered 条目）
shrink truncate          -> Clipped（EOF 之后部分回收，前缀保持 Registered）
最后一次 writable close  -> Dropped（仅当无 delalloc reservation）
写回结算 reservation 清零 -> Dropped（close 已推迟、只有只读句柄持有时兜住）
eviction                 -> Dropped（无条件，含 nlink 非零的 cache eviction）
```

一条 space 是"某文件的某段逻辑区间由这段物理 run 兑现"的许诺，只存在于内存：位图、group
descriptor 与 inode 都不记录它，块在被消费前不进 `i_disksize` 也不写盘，因此崩溃或卸载后
零磁盘残留。它同时登记在两个视图里，两侧只在同一个 allocator 临界区内一起变更——单侧
失效条目既保护不了任何块，也没有可交易的许诺：inode 侧按逻辑起始块索引供消费查找，group 侧
按 `(group, 起始物理块)` 索引供组扫描排除与 ENOSPC discard，且每个孪生条目持有 owner 的强引用。

守恒不变量：`preallocated_blocks` 恒等于 group 侧各孪生段长度之和；每次变更后由
`debug_assert_prealloc_conservation!` 校验。group 侧的强 owner 引用使 space 不可能早于
inode 对象释放，所以 eviction 必须无条件 discard，否则孪生表会留悬空条目。

free 口径不受 space 影响：`free_blocks_count()` 返回磁盘聚合值，未消费的 surplus 仍计为空闲
（Linux statfs 同口径），因此"聚合非零但选不出块"是正常状态，由 ENOSPC 重试阀处理，而不是
把 surplus 从 free 中扣除。

与 Linux 的差别：`MB_GROUP_PA`（inode PA 未命中时的 locality-group 回退）在此没有对应物。
kext4 没有 locality-group PA 生产者；写路径的长度归一化对小于 `EXT4_PREALLOC_MIN_WRITE_BLOCKS`
（4 块）的请求不做放大，因此小写入不产生 surplus，也就不登记 space。

**当前接线状态**：两侧索引、组扫描排除、ENOSPC 重试阀、shrink clip 以及 close/eviction 的
生命周期 discard 已接入生产路径；space 的**产出（birth）与消费（consume）入口尚无生产调用者**，
由写回 wiring 提交接入，在此之前生产路径不会登记任何 space，上述回收侧代码只由单元测试驱动。

## 算法流程

Namei 修改先验证 parent/name，查找目标 dirent，检查 inode kind 和磁盘格式约束，然后在
一个 journal transaction 中完成 dirent 和 inode 更新。Rename 使用准备、替换、删除、收尾
的顺序，保证目录父链接计数和 `..` 更新保持一致。

目录插入的额度按操作上界预留，查槽、append、HTree 转换和分裂由执行路径决定。
`DirectoryInsertPlan`、路径枚举及其派生字段已删除；额度计算不读取目录内容，也不依赖
空间预检选中了哪条路径。`Ext4Credits` 组合 inode、dirent、HTree 和 extent 各自的预算。
extent 层按 inode 已校验的当前树深计算一次 append 的新块与旧路径重写上界。一次插入
复用旧外部块，每层至多增加一个兄弟；inline leaf 外溢及 root 增高也计入上界。只有启用
索引且尚未索引的单块目录可能转换，才计入两次 append；第二次按至多增高一层预算。
目录是否使用 HTree 由文件系统特性和 inode index 标志共同决定，查找、扫描、修改和额度
计算共享该判定。分配器层导出的
`BLOCK_ALLOCATION_METADATA_CREDITS` 统一表示分配元数据加新块 create access，父目录增长
与 mkdir/长 symlink 的 child 块都复用该定义。

空间检查仍在 metadata 发布前校验目录和 extent 容量，仅返回所需块数。它保留原地插入在
无空闲块时仍可完成的行为，并避免当前没有局部回滚的 namespace 路径在分配失败后留下
半完成修改；它不再承担 journal 预算协议。转换后立即分裂仍按两个独立单块 mapping
检查 extent 空间，不能假设物理分配连续。
传统块指针目录仍可使用已有槽位；其 mapping 增长由容量检查在 admission 前拒绝，
不会为了预算而把块指针误解码成 extent root。

预算是保守上界，原地插入也申请同一操作额度，handle stop 归还未消耗部分。单次请求超过
journal 事务容量时返回 `NoSpace`，不 abort；多个 handle 的总额度恰好等于上限时允许加入，
超过上限时 journal 核心可等待已有 handle 退出。文件系统 mutation 在 mount 写锁下
串行执行，admission 使用现有 active-handle 计数拒绝嵌套，再持久化旧事务，因此不会
持 mount 锁等待自己的 handle。等待复用事务状态队列，stop、commit、checkpoint 和 abort
均通知该队列，不增加另一套 reservation 状态。

执行中额度耗尽仍 abort journal，当前调用对 VFS 返回 `ENOSPC`；后续 mutation 在 journal
入口返回 `JournalAborted`，映射为 `EROFS`，且不会先修改 journal 特性或 superblock 元数据。
journal 的 abort 状态是写入禁用依据。`Ext4MountWriteGuard` 借用调用方已有 superblock，
在释放 mount 写锁前将 abort 发布到其既有 `RDONLY` 标志，阻止后续缓冲写进入页缓存；
remount 为可写前同样校验 journal abort，失败不应用挂载选项或清除只读标志。缓存写入
校验与 delalloc reservation 也检查同一个 abort 状态，避免 VFS 标志更新窗口允许继续
标脏；不保存额外的 superblock 引用或 read-only 字段。

`ut-tests/namei/` 的预算回归覆盖原地插入、碎片化 inline/external extent 的 link/rename、
HTree 转换、转换后立即分裂及带外部 extent 的已有 HTree leaf split。特殊路径在各自额度
上限下完成并重开检查持久化；少一个 admission credit 时返回 `NoSpace`，不写盘、不改链接
计数且不 abort。`ut-tests/jbd2/transaction/` 验证精确容量、真实等待、stop/abort 唤醒；
`ut-tests/journal/` 与 `ut-tests/vfs/` 验证嵌套 admission、额度耗尽的 abort 边界、
remount 校验及缓存写入 errno。固定 64-credit 事务覆盖连续 hard link 跨越 HTree 转换，
断言仍共享同一 running transaction，并重开检查持久化。

目录磁盘语义由既有对象分层承担：`RawDirectoryEntry` 统一完成 `rec_len` 编解码，并与 Linux
`ext4_rec_len_from_disk()` 一致地把原始 `0` 和 `0xffff` 解释为整个目录块；64 KiB 整块写回使用
`0xffff`。`HTreeRootInfo` 统一校验 root 固定字段与磁盘 hash version，`Ext4SbInfo::decode_htree_root()`
再结合 `large_dir` 校验树深并解码 count/limit，读写路径不再各自维护一套 root 规则。磁盘 root
只接受 Linux 的 `0/1/2/6`。磁盘 `Ext4DiskSuperblock` 只保存 `s_def_hash_version`、`s_hash_seed` 和
`s_flags` 事实；`Ext4SbInfo` 在 mount 阶段按 `ext4_hash_info_init()` 校验默认版本，并且只在
启用 `DIR_INDEX` 时解释 signedness flags。显式 unsigned 优先，显式 signed 保持 signed；两者都
未设置时按当前 Linux 的全局 unsigned-char 语义生成运行态 `hash_unsigned=3`。该初始化阶段只
建立 mount state；可写 VFS mount 在 root inode 建立后、superblock 发布前的 finalize 边界把
`EXT2_FLAGS_UNSIGNED_HASH` 写回，read-only mount 不修改磁盘。这对应 Linux 先由
`ext4_hash_info_init()` 解析策略、再由 `ext4_setup_super()` 提交 superblock 的生命周期。
HTree 入口把 root 磁盘版本与该
运行态 offset 组合后执行算法，调用者不能用裸参数重新拼装策略。SIPHASH 因此是合法 root 格式，
但在 KExt4 尚无 fscrypt name key 时于首次 hash 计算前返回 `Unsupported`，不会被误报为目录损坏；
依赖该 hash 的 namespace transaction 因而不能选择或写入对应 HTree leaf，并以失败结束而不提交
metadata 变更。Orlov 顶层目录放置另走独立入口：Linux `find_group_orlov()` 为这一入口重新构造
`dx_hash_info`，固定指定 signed `DX_HASH_HALF_MD4` 和 `s_hash_seed`，不读取
`s_def_hash_version` 或 `s_hash_unsigned`。KExt4 保持同一边界，因此 LEGACY、TEA、HALF_MD4
默认版本的镜像都得到相同的 Orlov 起始 flex；这可能改变旧 KExt4 的目录放置位置，但属于恢复
Linux allocator 语义，不改变磁盘 HTree 格式。

Create、mkdir、mknod 和 symlink 的 kext4 KVFS 层 callback 接收同一次操作的 `&Cred`，先用
`inode_init_owner()` 根据父 inode、`fsuid/fsgid` 和 setgid 继承规则得到 mode/UID/GID，
再把显式 `uid`、`gid` 参数传入 KExt4 namei transaction。核心 inode constructor 不读取当前任务，
也不提供固定 root owner 的运行态默认值；测试镜像构造必须显式传入其 fixture owner。

Xattr 修改会把 inline xattr 和 external xattr 解码一次，在同一份 mutation plan 中完成
存在性检查、值更新、存储布局选择与 journal credits 计算。普通 set/remove 固定使用 inode
当前的 `i_extra_isize`，不会借 xattr syscall 机会扩大 extra fields：set 先尝试把发生变化的
entry 放入 inode body，放不下时只把该 entry 放入 single external block；无关 entry 保持原区域，
remove 也只删除目标 entry。提交时同时维护 `i_file_acl`、`i_blocks`、Linux-compatible external
entry hash、block hash、block checksum 和 refcount。Inline entry 的
`e_hash` 按 ext4 格式保持为零，external entry 则对 name 与四字节补齐后的 value 计算旋转异或
hash，block header 再按 entry 顺序聚合 `h_hash`。
`Ext4XattrSetMode` 表达无标志、create、replace 和 create+replace 四种组合；组合标志在属性
存在时返回 `EEXIST`，缺失时返回 `ENODATA`，不会通过锁外预查实现。允许替换时，
若现有值逐字节相同，core 在布局规划、journal handle、metadata write 和 ctime 更新前返回。
effective want 沿用 Linux 的 superblock 初始化策略：inode 大于 128 字节时先为 Linux 已知的扩展
inode 字段保留 32 字节；仅当文件系统声明 `RO_COMPAT_EXTRA_ISIZE` 时，再与磁盘 min/want
取最大值。新 inode 在分配事务中直接把该 effective want 写入 `i_extra_isize`，因此第一次
普通写回不会为了补齐新 inode 再读取和规划 xattr。已有 external 属性集合逐字节不变时直接
复用原 block，即使它是共享 block 且 allocator 已满也不触发 COW；只有 external 内容发生变化
时，才要求已有私有 block 可原地重写或 allocator 尚有空闲 block；因此磁盘已满但目标 entry
仍可在当前布局内完成的更新不会被误报为 `ENOSPC`。
`Ext4Inode` 从磁盘 `i_flags` 暴露 immutable 和 append-only 状态；kext4 KVFS 层在 iget 时把它们映射
为 KVFS `NodeFlags`，使通用 xattr 权限层在进入 namespace 或 KExt4 mutation 前返回 `EPERM`。

除 xattr 自身更新和 zero-link eviction 外，所有普通 inode dirty 入口都会先尝试把现有 inode
的 `i_extra_isize` 扩到 superblock 的运行时 effective `want_extra_isize`；这覆盖 regular-file
写入/截断、extent 更新、chmod/chown/utimes 以及目录 namei 元数据，而不是只覆盖文件写入。
Xattr apply 走底层 inode-table helper，避免扩展过程递归进入 xattr mutation。目标布局容纳不下
全部 xattr 时退到磁盘
`min_extra_isize`，仍不成立则保留当前大小而继续普通 metadata 更新。需要缩小 inline 区域时，
规划器按 Linux 的 entry-by-entry 策略重复扫描：优先迁移足以一次腾出所需空间的最小 entry，
否则迁移本轮扫描中最后一个 external block 可容纳的 entry 后继续；`system.data` 不允许外迁。
迁移、inode-body 重编码和 `i_extra_isize` 更新属于同一 journal transaction；handle 只为新增的
external metadata targets 扩展 credits，transaction engine 使用挂载时固化的 journal limit
拒绝越界扩展。
需要新 external block 时，在进入布局 apply 前先完成块分配；规划后的空闲空间竞争若返回
`NoSpace`，则记录 `no_expand` 并继续普通 inode metadata 更新。设备 I/O、checksum 或元数据损坏
仍按 journal 完整性错误传播，不能作为机会性扩展失败静默忽略。
确认没有可行扩展布局后，`Ext4Inode` 在 resident 私有状态中记录 Linux
`EXT4_STATE_NO_EXPAND` 对等标志，后续写入不再重复读取、校验和规划相同的 xattr；成功删除
xattr 会清除此标志，因为 inode-body/external 空间已经改变。`JournalBusy` 或 journal credits
暂时不足发生在实际布局迁移之前，不记录为永久失败，后续 transaction 仍可重试。

`list_xattrs()` 使用 `Ext4XattrNameSink` 逐项借用已校验的磁盘名称，只验证 value range 而不
复制 value。kext4 KVFS 层 在 sink 中添加 `user.*`、`trusted.*`、`security.*` 前缀并继续流式
传递，不构造 `Ext4Xattr` 或完整名称中间向量。成功 mutation 后 kext4 KVFS 层把 core inode ctime
同步回共享 VFS identity。Zero-link eviction 会复用 external xattr block 清理逻辑，先释放
EA block，再释放 inode bitmap entry。

Truncate 使用 orphan file（满时回退 legacy orphan list）保护 regular-file shrink。KExt4 的
`InodeOperations::set_len()` 按
`prepare_regular_inode_truncate()` → `VfsInode::truncate_setsize()`（先发布 VFS i_size，
再执行 unmap/cache truncate/unmap）→
delalloc extent-status 尾部删除 → `finish_regular_inode_shrink()` 排序。是否删除 delalloc
以旧 `i_size` 为准，是否释放磁盘 mapping 以旧 `i_disksize` 为准；因此截断到
`[i_disksize, old i_size)` 或恰好等于 `i_disksize` 仍会在 dirty folio 被丢弃后同步减少
`i_reserved_data_blocks` 与 mount aggregate。这与 Linux ext4 `setattr` 路径在
`i_size_write()` 后执行其内部 `truncate_pagecache()`、再进行 filesystem block truncate
的职责层次一致。`prepare_regular_inode_truncate()` 即使先提交了缩小后的 `i_disksize`，也必须
保留旧 `i_size`，使随后 PageCache 能按真实旧 EOF 丢弃 folio、清零同页尾部并失效映射；不增加
第二个 truncate operation hook。显式 recovery 在 journal 需要 replay 时先重放并保持 recovery flag，
再初始化 mount-lifetime orphan info 并遍历 legacy list 和 orphan file；即使 journal 已 clean，
只要 superblock 仍有 orphan head、orphan-file entry 或 `ORPHAN_PRESENT`，也会执行同一 cleanup。
`nlink > 0` regular inode 完成中断的 truncate，`nlink == 0` inode 复用 final
eviction 事务释放 external xattr、extent 和 inode bitmap。`recover()` 返回 `None` 只表示
没有 journal replay report，不表示没有执行 orphan cleanup，也不降低成功返回的持久性保证。
Orphan file backing inode 通过 Linux `EXT4_IGET_SPECIAL` 对等的内部入口加载，不进入 public
iget、inode allocator 或 release。`Ext4SbInfo::orphan_info` 只保存 Linux `ext4_orphan_info`
同层的 block identity、checksum seed 与增量 free-entry 计数；resident `Ext4Inode` 保存
`i_orphan`/`i_orphan_idx` 对等的 `OrphanLocation`，不把运行时索引塞进按需解码的磁盘 inode 值。
`OrphanInfo::add/remove` 直接拥有槽位选择、journaled block 更新和 free-entry 计数维护；
`Ext4SbInfo` 负责 orphan 格式分派、legacy fallback 和 recovery 调度。格式参数借用现有磁盘
superblock，I/O 借用现有 metadata object，不增加 operation context 或重复 geometry 字段。
挂载级独占访问串行化槽位修改，因此不需要 Linux 并发分配器的 CPU hash、CAS 和原子计数。
信息只在 journal replay 后初始化一次；recovery 每块只取一次 entry snapshot，逐 inode
checkpoint 只重载 superblock/group 可变状态并增量维护 free-entry 计数，不重复扫描整个 orphan file。
Legacy orphan 链的 `i_dtime` 是 journaled inode-table topology，不复制到 resident private
state。链遍历直接读取 metadata cache 中的 inode-table bytes，前驱重连也按已经校验的 inode
number 原位修改同一 bytes；它不会为了读取或更新前驱而构造另一个 `Ext4Inode`。因此两个同时
open-unlink 的 inode 可以按任意顺序完成 final eviction，而不会从旧 private snapshot 恢复已经
移除的 orphan next。
clean-journal cleanup 的首个 transaction 会先建立 ext4 recovery evidence；所有 recovery
cleanup transaction 都采用 `PreserveDuringRecovery`，逐个同步完成 commit/checkpoint，并从
已落盘的 superblock/group descriptors 重新建立内存状态，避免旧 orphan head 或 allocator
counter 被 checkpoint 前的快照重新带回循环。重建时冻结的 `group_geometry` 地址表保持
挂载时的值：replay 后的 descriptor 地址若与冻结表不一致，reload 直接报 corruption
返回，不会带着分裂的地址状态继续清理。全部 orphan 清理完成后，recovery 再确认 JBD2
`s_start` 为零，最后清除并 flush ext4 recovery feature；任一步失败都会返回错误，而不会先
清除最终的磁盘恢复证据。Ext4DiskSuperblock decode 会保留越过 inode table 的原始
`s_last_orphan`，使显式 recovery 有机会处理该损坏，而普通 mount 仍因非零 orphan head 返回
`NeedsRecovery`。Recovery 在 `iget` 前同时校验 inode table 上界、reserved inode 范围和经过
checksum 验证的 inode bitmap 分配位；编号合法但 allocation bit 为零的 head 不读取 inode
table。成功加载 inode 后还会在执行 truncate/eviction 前验证：带链接的 inode kind 必须属于
Linux ext4 可截断的 regular/directory/非 fast symlink，以及原始 `i_dtime` 不得超过 inode
上界；legacy next 还必须为零或合法的下一
orphan 编号。命中这些 bad-orphan 条件时按 Linux `ext4_orphan_cleanup()` 的终止规则，用独立的
一-credit transaction 把 head 清零。该 transaction 同样采用 `PreserveDuringRecovery` 并计为
cleanup work，避免 replay 分支把已经推进的 journal sequence 回写成旧状态；只有 clear 的
commit/checkpoint 完成后才允许清理最终 recovery feature。Bitmap 读取/checksum、inode decode、
合法但 KExt4 尚不支持的格式或 cleanup 错误仍向上返回，不借此路径静默吞掉一般 metadata
corruption 或把未完成的有效 cleanup 伪装成成功。

Truncate 和 unwritten preallocation discard 的 journal credits 按实际 extent 结构计算：inode
root、重建后的 extent-tree blocks、需要 revoke 的旧 tree blocks，以及释放范围覆盖的不同 block
group 中各一个 bitmap/descriptor target。此处的 "unwritten preallocation" 指磁盘 extent tree 中
越过 `i_disksize` 的未写 extent（`discard_regular_inode_preallocations()`，对应 Linux
`ext3_truncate_failed_write()` 一类清理），与上一节"写回 speculative prealloc space"所述的
in-memory space 无关：后者不写盘、不需要 credits。数据块数量本身不会一对一增加 journal metadata block，
因此不能用 `i_blocks` 或被释放 data block 数直接放大 reservation；否则大文件只回收一个很小的
preallocation tail 也会被误判为超过空 journal 容量。唯一例外是目录与 block-mapped symlink
的数据块：它们本身是 journaled metadata buffer，释放路径对每个被释放块各产生一个 revoke，
因此 credits 按本次实际释放的数据块数逐一追加 revoke credit（与 Linux
get_default_free_blocks_flags() 对 S_ISDIR/S_ISLNK 的 METADATA|FORGET 语义一致）；regular
file 数据块不带该开销。计算结果仍为 allocator entry check 保留固定 headroom，并在任何
metadata mutation 前完成。

Ordered writeback 的 insert 与 unwritten conversion 只使用 `ExtentPath`，因此 transaction 内不再
切换到复杂度取决于整棵树大小的重建算法。writeback execution cursor 每步读取 live mapping；
credits 对每个实际 unwritten conversion 计一次 extent mutation，对 allocator 实际返回的每个
hole run 计一次 allocation、insert 和 conversion。Mapped overwrite 只保留最终 inode metadata
余量，不再随 data blocks 线性放大。计算仍覆盖 extent 最大深度和每层 split 可能涉及的
existing/new metadata targets，且不使用固定数值截断。每次 mutation 开始前，handle 必须保留
该操作增量以及最终 inode publish 的固定余量；容量不足时只能在前一个 run 完整结束后重启
transaction，不能在 path-local mutation 中途拆分。跨叶 range removal 的全树回退仍使用
truncate planner 按实际 tree blocks、revoke targets 和 affected groups 单独估算。

分段 writeback 的返回值采用“可能部分完成”的契约：后续 transaction 失败不会撤销已经提交的
durable prefix，也不会把其 delalloc reservation 重新计入。core 把已完成字节前缀经 KVFS 返回
给 MappingPages，KVFS callback 使用 `Result<usize, (usize, VfsError)>` 直接表达
提交长度或完成前缀与错误，不再包装 WritebackRangeOutcome；完全落在该前缀内的 folio 结束 writeback，跨越前缀边界的 folio 和剩余后缀保持
dirty。再次提交时只重试 dirty suffix，并从 live mapping 重新判断当前状态。普通可重试错误
允许在当前 mount 重试；device 或 JBD2 错误则永久 abort 当前 mount journal，恢复路径是
recovery/remount 后重新提交，不能声称同一 mount 上必然收敛。

`huge_file` superblock feature 表示 inode 可以使用扩展的 block accounting 格式；未设置
`EXT4_HUGE_FILE_FL` 的普通 inode 仍以 512-byte sector 记录 `i_blocks`，KExt4 可以安全修改。
真正设置该 inode flag、以 filesystem block 为单位计数的 inode 仍显式返回 unsupported。

Namei、setattr、writeback 和 truncate mutation 原位更新 VFS inode 组合持有的 ext4 private
state；完整 `stat/getattr` 通过 Linux `struct kstat` 对等的瞬时 `Ext4InodeStat` 在一次 inode-state
临界区读取 nlink、size、blocks、mode/owner、rdev 和 timestamps。该值不驻留、不参与 identity，
也不是 attribute cache。unlink/rmdir/rename 从已锁定 dentry 取得 victim/moved/replaced VFS
inode 并把其 private state 显式传给 core，禁止 core 在 mutation 中按 inode number 重新加载。
VFS-wide identity table 因而以 `(SuperBlock, inode number)` 保证一个 live identity 只对应一个 `VfsInode`、一个
`AddressSpace` 和一份 ext4 private state。

`Ext4SbInfo::timestamp_limits()` 从 filesystem inode size 推导共享的
`ktime_types::TimestampLimits`：不能容纳 `i_atime_extra` 的 128-byte inode 只支持整秒及
有符号 32-bit 秒范围；具备完整 extra timestamp 空间的 inode 支持纳秒和扩展 epoch。ext4
superblock operations 在 fill 期间从已经安装的 `Ext4SbInfo` 返回该能力，由唯一的 KVFS
`SuperBlock` 持有；bridge 不再重复保存或转换平行的范围结构。VFS 生成当前时间时先按该能力截断，再用于比较、filesystem callback 和
resident publication；raw inode setter 只保存已经准备好的值。新建 inode 的初始化结构保存
完整 `Ext4Timestamp`，并通过与存量 inode 相同的 base/extra encoder 写入 atime、ctime 和
mtime，因而 256-byte inode 保留纳秒与 epoch 位，128-byte inode 在磁盘边界截断到可表示的
整秒范围。既有 timestamp encode/decode 仍按每个实际 extra field 是否存在进行截断或校验，
作为磁盘格式边界的第二层保护。

挂载时由 ext4 fill operation 把 filesystem block size 与 extent-format `s_maxbytes` 写入 KVFS
`SuperBlock`，legacy bitmap maxbytes 只保存在 `Ext4SbInfo`；Ext4Inode 通过其组合持有的 ext4
private state 查询 extent-format 状态。write、truncate、FIEMAP
和 `page_mkwrite` 通过同一个 helper 按该格式状态选择上限，既不复制派生的 per-inode
maxbytes，也不为上限查询重新读取 inode-table 或取得挂载级 core lock。
Block mapping 热路径从同一 inode state 临界区一次取得 ext4 flags 与 60-byte `i_block` root；
extent 和 legacy mapper 随后借用该局部 root 完成一次 run 查询，避免 direct-pointer 合并循环
重复加锁或复制 `i_block`，也不在块设备 I/O 期间持有 inode state lock。
每份 inode private state 用不重叠的 logical-block 区间保存 delayed extent，
对应 Linux `ext4_inode_info::i_es_tree` 中的 delayed entries，并维护
`i_reserved_data_blocks` 等价计数。挂载级 reservation aggregate 对应 Linux
`s_dirtyclusters_counter`，用于 admission 与 `statfs()`，不是第二份 extent identity。
Delayed-allocation admission 使用 primary superblock 的 free-block counter 减去 ext4 reserved
blocks 和 core mount aggregate。reserve/release/truncate/writeback/eviction 只能调用 core 的
区间 API，由 core 在各自临界区内原子更新 inode 区间、per-inode count 和 mount
aggregate（inode 侧在 per-inode 状态锁内，mount 总量在独立 delalloc 锁内）；
bridge 不读取或调整任一计数。该 counter 与 group descriptor 由同一
allocation/release mutation 更新，因此 admission 与显式 `statfs()` 都直接读该常
数时间 aggregate，不遍历 group descriptor（避免持锁 O(组数) 折叠阻塞全局分配）。
目录分配的 Orlov goal 选择同样只读常数时间 aggregate：allocator 锁内一次取得
free-inodes/free-blocks 与 mount 级 directory-inode 总量（对应 Linux
`s_dirs_counter`，挂载时一次 O(组数) fold 播种，此后与逐组 `bg_used_dirs_count`
同点更新），不在持锁期间逐组折叠。

Core 通过 `Ext4StatFsMode` 暴露两种 Linux ext4 总容量口径，但不解析 mount-option 字符串：
`Bsd` 从 superblock 总块数中扣除 first-data-block 与 metadata system zones，`Minix` 直接使用
on-disk `blocks_count`。两种模式共享同一次 group free-inode/free-block 聚合，并同样扣除
reserved blocks 与 delayed-allocation mount aggregate，因此只允许 `Ext4StatFs::blocks`
不同，`blocks_free`、`blocks_available` 和 inode 统计必须一致。模式的选择与生命周期由 VFS
context 从 opaque mount data 解析后写入唯一 `Ext4SbInfo`，对应 Linux
`ext4_sb_info::s_mount_opt`，不放入静态 operation table，也不增加 wrapper。新挂载未指定时默认
`bsddf`；reconfigure 未指定该选项时保留现值，显式 `bsddf`/`minixdf` 才更新 mount-private state。
Linux 默认启用的正向选项 `user_xattr` 和 `acl` 作为 mount-data 兼容拼写被接受，但不创建未被
消费的 per-mount 字段；其中 `acl` 的语法兼容不扩大当前 KVFS POSIX ACL 权限语义。

Buffered write 在 `FileOperations::write_iter()` 中、generic write 的 inode data critical
section 内应用 inode-format 上限，再由 `write_begin()` 查询 core mapping。shared-file write
fault 则在 address-space invalidate shared lock 和 folio lock 内应用同一上限并调用
`page_mkwrite()`。两个入口都会把 hole block 加入同一个 delayed set；因此非 `SYNC`
FIEMAP 能统一报告两种写入口产生的 `DELALLOC | UNKNOWN`，不扫描 dirty folio猜测 allocation
状态。

FIEMAP 查询通过 inode operation 进入，并复用与 write/truncate 相同的已缓存 inode 格式上限：
extent 格式按 `(2^32 - 1) << block_bits` 及 `i_blocks` 上限计算，legacy
格式同时计入 indirect metadata blocks、`huge_file` 与 `MAX_LFS_FILESIZE` 上限。随后按请求
范围调用只读 `report_mapping()`；它对应 Linux `ext4_iomap_begin_report()` 经
`ext4_map_blocks()` 观察 `EXT4_MAP_DELAYED` 的层次，在 core 内把 inode extent-status 区间覆盖到
磁盘 hole 的 `BlockMappingFlags::DELAYED`。私有 VFS 转换函数只遍历统一 mapping 结果：普通 hole 被跳过，
mapped extent 输出物理块范围，unwritten 添加 `UNWRITTEN`，delayed hole 添加
`DELALLOC | UNKNOWN`。Legacy pointer 映射按连续 logical/physical block 合并并添加
`BlockMappingFlags::MERGED`，kext4 KVFS 层将其转换为 `FIEMAP_EXTENT_MERGED`。遍历保留一个
pending extent，只有确认查询范围内没有后续映射时才添加 `LAST`；输出容量满时立即停止且
不误标 `LAST`。

块分配入口分两层：`balloc` 的单组分配（`allocate_block_run_in_group()`）与 `mballoc` 的多组扫描
（`allocate_blocks_for_write()`）。只有多组扫描接 ENOSPC 重试阀：组扫描可能因为 prealloc space
占位而选不出块——位图扫描把这些位当作受保护位排除，而 free 聚合仍把它们计为空闲——此时先
discard 若干 group 的 space 再重扫；单组入口不重试，因为 discard 按组号升序、无法保证触达请求的
组，全局丢弃只会损失其他文件的局部性。重试上界由 `PREALLOC_DISCARD_SEARCH_LIMIT`（3 次搜索）固定：
discard 只可能出现在第一次之后的每次搜索之前，且 discard 返回 0（无 space 可丢）时立即结束，
因为重扫结果必然相同。discard 以整组为粒度、按组号升序累计到 `needed` 为止，因此可能超出请求量；
`needed == 0` 表示清扫所有持有 space 的组。与 Linux 的 `freed > 0 || discard_pa_seq 前进` 双重门
不同，这里只保留 `freed > 0`：单把 allocator 互斥锁已经串行化了选取与 discard，不存在其他 CPU
在调用者背后释放 space 的情形。重试只在出现"健康组报告无空间"时发生：几何或描述符预检跳过的组
不置位，因为丢弃改变不了组容量；扫描中遇到的 `InvalidBlockBitmap` 会被记住并优先于 `NoSpace`
返回，但不会禁止健康组在回收后重试。阀自带 allocator 锁，因此必须在扫描的锁段
之外调用，否则会重入死锁。

## 并发模型

运行态仍需联合修改 superblock/group metadata 的 mutation 当前通过 `Ext4Mount` 内的挂载级 `RwLock`
write guard 串行化；只读调用以及 delalloc reservation 可共享 read guard，后者再使用独立
reservation mutex。KVFS inode cache mutex 只保护 `New/Live/Freeing` identity state；每个 cache
slot 使用自己的等待队列，等待发生在释放 cache mutex 之后，因此一个 inode 的状态变化不会
唤醒其它 inode 的 `iget`。ext4 private state 使用独立 sleepable mutex，普通 metadata 读取或
mutation 不进入另一张 inode cache。
核心内部的 metadata buffer
和 JBD2 transaction handle 仍会记录 buffer ownership、credit consumption 和 revoke 状态。
同一 inode 的 `writepages()` 由 `Ext4Inode` 的 sleepable writeback mutex 串行化，但进入 PageCache
遍历时不持有挂载级 core mutex；PageCache 在释放 mapping/folio mutex 后调用 batch writer，
batch writer 才短暂取得 core write guard；delalloc accounting 由独立 reservation mutex 更新。这样
普通 cache miss 的 `MappingInner -> core` 路径不会与 writeback 形成反向锁序。
FIEMAP 在 VFS inode shared lock 下执行，regular inode 另以 `writeback_lock` 稳定磁盘
mapping；core `report_mapping()` 只短暂读取 delayed extent lock，随后再查询磁盘 mapping，
并在调用安全输出 writer 前释放 core 锁。Buffered write 和 truncate 使用 inode data lock 的 exclusive 侧；
`page_mkwrite` 使用 address-space invalidate shared lock 和 folio lock，truncate 同时使用
invalidate exclusive 侧。三条路径通过同一个 delayed-set lock 发布 mapping/reservation
状态。用户页错误或大输出因此不会持有挂载级 core read lock。
N1 已将 journal mapping/superblock、transaction engine 和 checkpoint queue 固定到同一
`MountedJournal`。N2 才根据后台 commit/checkpoint、inode writeback、metadata buffer 和 group
allocator 的实际并发关系建立锁顺序；不以字段分组预设锁域。不得在 spinlock 下执行块 I/O、
等待 PageCache 或获取 sleepable lock。

## 设计决策

- ext4 磁盘格式、一致性不变量和 inode component 由 `kext4` 核心负责；resident identity、
  generic attribute 语义、PageCache 和 open-file 引用生命周期只由 KVFS `VfsInode` 负责。
  KVFS 通过 attribute operations 访问该组件里的唯一通用属性存储，不维护第二份 cache。
- `kext4` crate 使用 `#![forbid(unsafe_code)]`，unsafe 或设备相关细节留在核心边界之外。
- 未实现的 ext4 格式能力通过显式 unsupported error 暴露，避免把不完整格式误挂载为可写。
- KExt4 的新生命周期与 I/O 语义只在 KExt4 直接 KVFS 实现中落地，不保留第二套 ext4 实现路径。
- KExt4 core 只提供 inode 格式相关的最大文件大小和带 mapping flags 的 `BlockMapping`；Linux FIEMAP ABI、
  用户指针与输出容量留在 POSIX/KVFS 边界，`vfs` 模块只负责把 mapping 语义转换为 extent。
- `Ext4SbInfo` 保留类似 Linux `ext4_sb_info` 的 mount 总状态；只有具有独立事务状态机和
  生命周期不变量的 journal 聚合为 `MountedJournal`，不为代码分组机械创建 service。
- KExt4 持有真实 `BlockDevice`，通过公共同步适配提交块读写和 flush；request、完成通知和 VirtIO
  中断队列属于 block/driver 层。KExt4 可合并请求并在通用接口可用后接入，但不建立私有驱动
  旁路。当前 transaction restart 仍需同步 `flush` 建立 ordered-data durability 边界；只有通用
  block 层提供 request completion/FUA 或等价 dependency 后，才能把多个 transaction 的数据与
  commit 安全流水化，不能在 filesystem 层直接删除该 barrier。
- speculative prealloc space 只做内存记账：不在位图、也不在 in-core free-extent cache 中预先
  打标（Linux 会给 buddy 打标并要求与 PA 生命周期同步），两侧索引是唯一记录。代价是 space 对
  free 聚合不可见，"聚合非零但选不出块"因此成为正常状态，由 ENOSPC 重试阀处理而非改记账口径。
- 不实现 `MB_GROUP_PA`：kext4 没有 locality-group PA 生产者，且小于 `EXT4_PREALLOC_MIN_WRITE_BLOCKS`
  的请求不被归一化放大、不产生 surplus，小写入因此不登记 space，不需要 inode PA 未命中时的
  组级回退。
- shrink truncate 按新 EOF 裁剪 space，保留其下的前缀（Linux 在这里丢弃整个文件的
  preallocations）：kext4 的 space 带精确逻辑区间且有需要同步的 group 孪生条目，前缀仍然有效可用。
- ENOSPC discard 以整组为粒度，不部分释放一个组的 space：保持"要么整段许诺还在、要么整段消失"
  的简单不变量，避免为一个请求打散多个组的连续区间。
- errseq、clean unmount/freeze 和完整 fault matrix 依赖最终的后台执行图，集中放在 N3；它们
  不阻塞 N2 主路径，但仍是替换旧后端前的强制门槛。

## Drop / 资源释放

Namespace removal 在修改目录项之前验证 inode 的数据回收格式。`unlink`、`rmdir` 和覆盖式
`rename` 在链接数归零时分配 orphan-file slot，满时使用 legacy list；删除仍有其他硬链接的
普通 inode 不分配 orphan entry。最后一个 VFS 引用释放前，inode bitmap 和数据仍保持分配。

已分配的 metadata/data blocks 通过 journaled bitmap helper 释放。Inode 删除路径先切断
目录可达性，用 orphan file 或 legacy fallback 保护 zero-link cleanup；若 inode 带 external xattr
block，则先释放或降低 refcount，并清理 `i_file_acl`/`i_blocks`，然后 truncate
extent-backed data，清理 inode metadata，最后释放 inode bitmap entry。

运行态 kext4 KVFS 层仅在最后一个 writable-file `release()` 且没有 delayed data
reservation 时丢弃 EOF 后未使用的预分配，对应 Linux `ext4_release_file()`；
close 不额外强制普通 dirty PageCache writeback，数据回写由 `fsync`/`syncfs` 和通用
writeback 路径负责。`VfsInode` 最后一个引用消失时，superblock hook 先丢弃
PageCache/剩余 delalloc accounting，再对 nlink=0 inode 组合持有的 ext4 private state 调用
final eviction。KVFS 在 hook 前完成 `Live -> Freeing`，所以 cleanup 期间没有普通 VFS 能力；
三阶段 core API 始终借用该 `VfsInode` 组合持有的同一 private component；它不返回可逃逸的
eviction token 或另一种 inode handle。完成后 KVFS 精确删除旧 cache entry 并唤醒同号 `iget`。
nlink 非零的 cache eviction 不释放磁盘 inode；后续 `iget` 可从仍分配的磁盘 inode构造新的
private component。

speculative prealloc space 的回收挂在 file/inode 生命周期与写回结算上。最后一次 writable close 只在
没有 delalloc reservation 时丢弃该文件的全部 space；有 reservation 时保留，由写回结算或 eviction
兜底（对应 Linux `ext4_release_file()` 的 `i_reserved_data_blocks` 门）。写回结算在 reservation
清零后补一次回收，覆盖"close 已返回、只剩只读句柄、eviction 不会发生"的场景。eviction 在检查 nlink
之前丢弃全部 space，包括 nlink 非零的 cache eviction：group 侧孪生条目持有 owner 的强引用，漏丢
即让条目活过其 owner 对象。无 space 的 inode 由 per-inode map 的叶子锁判空，在取 allocator 互斥锁
之前早退，因此写回结算不会为一次空回收触碰全局 allocator 锁。各回收点都只更新内存索引并把块退回
可分配池，不产生磁盘写入、不需要 journal credits；块在被消费前从未在位图中打标，因此不存在需要
回退的磁盘状态。

Recovery 不创建 resident inode 表。它在 mount/VFS identity 尚未发布时通过 orphan-aware decode
取得临时 private state，复用正常 truncate 或 zero-link eviction。若 cleanup 失败，恢复证据
保留并阻止该 filesystem 被当作成功挂载。
