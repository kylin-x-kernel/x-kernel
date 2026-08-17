# KExt4 — 设计文档

## 定位

`fs/filesystems/kext4` 是 X-Kernel 的受检 ext4 存储核心。它负责 ext4
磁盘格式解析、元数据校验和、JBD2 恢复与事务、extent、分配器、truncate/orphan、
namespace mutation、xattr 以及 ordered-data writeback。

`fs/bridges/kext4_vfs` 是当前运行态 KVFS 适配层。它把 KExt4 核心暴露为 KVFS 的
superblock、inode、address-space 和 file operations，但不拥有 ext4 磁盘格式和
一致性不变量。

磁盘 superblock 的 compat、incompat 和 ro-compat feature 字段分别使用独立的
`bitflags` 类型表示。解码使用 `from_bits_retain` 保留未知磁盘位，避免把不同 feature
类别中数值相同的位混为一类，并保证挂载协商仍能报告原始 unsupported bits。

## 背景

KExt4 是 X-Kernel 唯一的 ext4 后端，保持 Rust 受检代码、明确的 feature negotiation、
日志化元数据更新，以及 e2fsprogs/e2fsck 互操作能力。`KFEAT_FS_EXT4` 直接把 KExt4
core/bridge 链入运行态，不再经过实现选择层。N0 已补齐 KVFS 单一 resident inode identity、
cached attributes、两阶段 truncate 和 open-unlink/final-evict 基线。N1 已建立 persistent journal
和 mount/journal 生命周期边界，N2 继续建立 buffered 并发执行框架，再在 N3 集中补齐 crash
recovery、错误观察和 unmount/freeze。

## 范围

主要源文件：

```text
fs/filesystems/kext4/
├── src/lib.rs
├── src/superblock.rs
├── src/journal.rs
├── src/jbd2/
├── src/buffer/
├── src/extent/
├── src/balloc.rs
├── src/ialloc.rs
├── src/mballoc.rs
├── src/file.rs
├── src/truncate.rs
├── src/orphan.rs
├── src/namei.rs
├── src/dir.rs
├── src/dirhash.rs
└── src/xattr.rs
```

运行态适配层位于 `fs/bridges/kext4_vfs/src/`。

## 架构

```text
KVFS syscall / PageCache
    |
    v
fs/bridges/kext4_vfs
    |
    v
Ext4Filesystem (mount state，类似 ext4_sb_info)
    |-- layout / feature negotiation / device
    |-- metadata buffer / checksum validation
    |-- group descriptors / allocator state
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
block::BlockDeviceOperations
```

核心修改路径通过 `JournalHandle` 进入事务，先经 buffer 层取得元数据创建/写入访问权，
再修改元数据字节和内存中的布局状态。`MountedJournal` 是生产路径唯一的 journal identity：
它拥有磁盘 superblock、ring 运行态和 transaction 状态。一个 transaction 对象依次经历
Running、Committing、Checkpoint 和 Finished phase；FIFO 队列只保存该对象本身，持久化证据
属于它的 Checkpoint phase，不复制 commit payload，也不保存指回另一 coordinator 的引用。
普通 mutation 不负责同步完成 home-block checkpoint。

## 调用约束 / 执行上下文

KExt4 核心 API 可能执行块设备 I/O、内存分配、JBD2 事务、checkpoint 和设备 flush，
因此属于任务上下文 API，允许阻塞，不适合在中断上下文调用。它也不适合在块设备、
分配器和 journal 状态尚未可用的早期启动阶段调用。

当前运行态 bridge 使用挂载级 `RwLock<kext4::Ext4Filesystem>`：只读调用可共享进入，
mutation 仍由 write guard 串行化。VFS inode 组合持有的 ext4 private state 以 per-inode
`RwLock<Ext4InodeState>` 保护磁盘 metadata working state 和 delayed-allocation extents：
只读路径（`size`/`stat`/`metadata_snapshot`/delalloc 查询）通过 `with_state` 以 read guard
共享进入，mutation 通过 `update_state` 以 write guard 独占；resident lifecycle 完全归 KVFS。
调用者不能假设 allocator 或 journal
已有 per-group 级别的并行修改。可写文件打开计数由
KVFS `VfsInode` 拥有，bridge 只在 release callback 中读取它。

该挂载级读写锁是过渡实现而不是长期调用契约。N1 已把磁盘 journal、transaction engine 和
checkpoint queue 收进同一个 `MountedJournal` 生命周期边界；metadata、allocator、device 和
geometry 继续由 `Ext4Filesystem` mount state 持有，对应 Linux `ext4_sb_info` 的聚合角色。
N2 根据真实执行者建立 journal、per-inode、metadata-buffer 和 per-group 锁，替代 bridge
全局串行化。所有这些路径仍属于可阻塞的任务上下文，不能从中断上下文调用。

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
解码 `RawInode` 并构造 bridge private state；初始化失败会撤销 slot 并唤醒等待者。最后一个
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
writeback 前领先 `i_disksize`，两者是 Linux 本来就有的不同字段。bridge 不再执行 mutation
后的 metadata snapshot 回灌，也不创建备用 resident wrapper 或按 inode number 重载 live state。
Regular-file metadata publish 只提交 `i_disksize` 等 ext4 状态，不能因为旧 `i_size ==
i_disksize` 就顺带修改 `i_size`；后者只由 VFS `write_end_set_size()` 或
`truncate_setsize()` 在 PageCache 顺序点发布。目录、符号链接等由 ext4 算法直接改变长度的
对象则在对应 metadata mutation 中显式发布其可见长度。

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

1. Handle 加入 mount-wide running transaction，并根据 mutation 类型预留 journal credits。
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
镜像测试覆盖了两个 committed transaction 同时可扫描、逐个推进 tail、最终 clean 和 e2fsck。
若 transaction 包含 primary superblock，persist 路径会把 recovery feature 同时合并进 journal
记录和该 transaction 的 frozen checkpoint image；因此较老 checkpoint 在后续 commit 仍 pending
时不会把磁盘 `needs_recovery` 错误清零。只有 journal tail 真正清空后才单独清除该标志。

普通 mutation 在 handle 内决定成功或失败，并可与后续 mutation 共享同一个 running
transaction。新 handle 加入前按 journal 格式开销采用约三分之一日志容量作为普通 transaction
上界；handle stop 会归还未使用 credits，因此 outstanding credits 表达仍在事务中占用的真实
容量。不再以固定 operation 数或“半个 journal”触发普通提交。home-block checkpoint
仍留在 FIFO queue；`syncfs` 和 KVFS unmount writeback 会先提交当前 running transaction，
再 drain 全部 pending checkpoint；普通 mount 在 dentry eviction 后再次执行同一同步路径，
以覆盖 final inode eviction 产生的新 metadata。journal 空间不足时提交者同步推进最老
checkpoint 后重试 append。当前仍由调用者同步驱动，没有 background worker 和基于时间的
age trigger。

精准 `fsync`/`fdatasync` transaction id 的所有权不在 journal 的 inode-number 全局表，而应在
VFS runtime inode identity 上，对应 Linux inode 内的 sync/datasync tid。现有 KVFS inode 尚未
提供该运行态字段以及“mutation 完成后发布 tid”的接口，因此 KExt4 当前采用保守语义：bridge
先回写目标 inode 的 PageCache，core 再提交当时的整个 running transaction 并 flush 设备。
这保证 durability，但可能连带提交无关 inode 的 metadata。待共享 VFS runtime inode 接口具备
后，再实现目标 transaction 等待；不能重新引入按 inode number 索引的 mount-wide cursor map。
`syncfs` 和 KVFS unmount writeback 仍提交 running transaction 并 drain 全部 checkpoint。
当前 ordered-data dependency 是同步基线：数据块写入发生在使其可达的 metadata transaction
完成之前；异步 dependency 对象和后台 writeback 属于后续阶段。

operation savepoint、operation token 和 operation-local metadata byte copy 已删除。ext4
mutation 在首次 metadata access 前完成格式、目标状态、空间、credits 和 extent path
可表达性检查；allocator 在私有 bitmap/descriptor/superblock bytes 与 free-extent cache 副本上
完成计算后再发布。JBD2 handle 只维护 credits 和 metadata/revoke membership，显式 stop
归还未使用 credits 并返回 accounting 错误；journal 自身维护 abort 状态。多个 handle 可独立
stop。每次成功 metadata/revoke access 都标记当前 handle 已发布更新，即使对应 block 已由
同一 running transaction 的前序 handle 加入；transaction membership 一旦发布，就不能由某个 handle
按路径局部删除，因为其他 handle 可能已共享同一 metadata block；后续 metadata access 失败会
abort journal。设备/checksum/状态机错误，以及任何发生在 metadata 已发布后的普通错误，同样
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
  -> nlink == 0 时加入 legacy orphan entry，并持久化 zero-link metadata
  -> 返回更新后的 parent/target inode 给 bridge

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
bridge 则继续使用有界的三阶段 eviction。

## 算法流程

Namei 修改先验证 parent/name，查找目标 dirent，检查 inode kind 和磁盘格式约束，然后在
一个 journal transaction 中完成 dirent 和 inode 更新。Rename 使用准备、替换、删除、收尾
的顺序，保证目录父链接计数和 `..` 更新保持一致。

目录插入预检返回一份同时供空间检查和 journal credits 使用的计划，区分原地插入、线性
append、已有 HTree leaf split、线性目录转 HTree，以及转换后立即 split。最后一种路径实际
执行两次独立的单块分配，因此 extent 预检在同一份临时叶子状态中加入两个最坏情况下不合并
的单块 mapping，不能把它们视为一个连续的两块 extent。计划一旦进入 HTree 路径，即使事务
开始时 inode 尚未设置 `EXT4_INDEX_FL`，credits 也包含 HTree 更新余量。

Create、mkdir、mknod 和 symlink 的 KVFS bridge callback 接收同一次操作的 `&Cred`，先用
`inode_init_owner()` 根据父 inode、`fsuid/fsgid` 和 setgid 继承规则得到 mode/UID/GID，
再把显式 `uid`、`gid` 参数传入 KExt4 namei transaction。核心 inode constructor 不读取当前任务，
也不提供固定 root owner 的运行态默认值；测试镜像构造必须显式传入其 fixture owner。

Xattr 修改会把 inline xattr 和 external xattr 解码一次，在同一份 mutation plan 中完成
存在性检查、值更新、存储布局选择与 journal credits 计算，再选择 inode-body 或 single
external-block 存储，维护 `i_file_acl`、`i_blocks`、block checksum 和 refcount。
`Ext4XattrSetMode` 表达无标志、create、replace 和 create+replace 四种组合；组合标志在属性
存在时返回 `EEXIST`，缺失时返回 `ENODATA`，不会通过 bridge 的锁外预查实现。允许替换时，
若现有值逐字节相同，core 在 journal handle、metadata write 和 ctime 更新前返回原 inode。
`Ext4Inode` 从磁盘 `i_flags` 暴露 immutable 和 append-only 状态；bridge 在 iget 时把它们映射
为 KVFS `NodeFlags`，使通用 xattr 权限层在进入 namespace 或 KExt4 mutation 前返回 `EPERM`。

`list_xattrs()` 使用 `Ext4XattrNameSink` 逐项借用已校验的磁盘名称，只验证 value range 而不
复制 value。KVFS bridge 在 sink 中添加 `user.*`、`trusted.*`、`security.*` 前缀并继续流式
传递，不构造 `Ext4Xattr` 或完整名称中间向量。成功 mutation 后 bridge 把 core inode ctime
同步回共享 VFS identity。Zero-link eviction 会复用 external xattr block 清理逻辑，先释放
EA block，再释放 inode bitmap entry。

Truncate 使用 legacy orphan list 保护 regular-file shrink。KExt4 的
`AddressSpaceOperations::set_len()` 按
`prepare_regular_inode_truncate()` → `AddressSpace::truncate_setsize()`（先发布 VFS i_size，
再执行 unmap/cache truncate/unmap）→
delalloc extent-status 尾部删除 → `finish_regular_inode_shrink()` 排序。是否删除 delalloc
以旧 `i_size` 为准，是否释放磁盘 mapping 以旧 `i_disksize` 为准；因此截断到
`[i_disksize, old i_size)` 或恰好等于 `i_disksize` 仍会在 dirty folio 被丢弃后同步减少
`i_reserved_data_blocks` 与 mount aggregate。这与 Linux ext4 `setattr` 路径在
`i_size_write()` 后执行其内部 `truncate_pagecache()`、再进行 filesystem block truncate
的职责层次一致。`prepare_regular_inode_truncate()` 即使先提交了缩小后的 `i_disksize`，也必须
保留旧 `i_size`，使随后 PageCache 能按真实旧 EOF 丢弃 folio、清零同页尾部并失效映射；不增加
第二个 truncate operation hook。显式 recovery 在 journal 需要 replay 时先重放并保持 recovery flag，再遍历
legacy orphan list；即使 journal 已 clean，只要 superblock 仍有 orphan head，也会执行同一
cleanup。`nlink > 0` regular inode 完成中断的 truncate，`nlink == 0` inode 复用 final
eviction 事务释放 external xattr、extent 和 inode bitmap。`recover()` 返回 `None` 只表示
没有 journal replay report，不表示没有执行 orphan cleanup，也不降低成功返回的持久性保证。
Legacy orphan 链的 `i_dtime` 是 journaled inode-table topology，不复制到 resident private
state。链遍历直接读取 metadata cache 中的 inode-table bytes，前驱重连也按已经校验的 inode
number 原位修改同一 bytes；它不会为了读取或更新前驱而构造另一个 `Ext4Inode`。因此两个同时
open-unlink 的 inode 可以按任意顺序完成 final eviction，而不会从旧 private snapshot 恢复已经
移除的 orphan next。
clean-journal cleanup 的首个 transaction 会先建立 ext4 recovery evidence；所有 recovery
cleanup transaction 都采用 `PreserveDuringRecovery`，逐个同步完成 commit/checkpoint，并从
已落盘的 superblock/group descriptors 重新建立内存状态，避免旧 orphan head 或 allocator
counter 被 checkpoint 前的快照重新带回循环。全部 orphan 清理完成后，recovery 再确认 JBD2
`s_start` 为零，最后清除并 flush ext4 recovery feature；任一步失败都会返回错误，而不会先
清除最终的磁盘恢复证据。

Truncate 和 unwritten preallocation discard 的 journal credits 按实际 extent 结构计算：inode
root、重建后的 extent-tree blocks、需要 revoke 的旧 tree blocks，以及释放范围覆盖的不同 block
group 中各一个 bitmap/descriptor target。数据块数量本身不会一对一增加 journal metadata block，
因此不能用 `i_blocks` 或被释放 data block 数直接放大 reservation；否则大文件只回收一个很小的
preallocation tail 也会被误判为超过空 journal 容量。唯一例外是目录与 block-mapped symlink
的数据块：它们本身是 journaled metadata buffer，释放路径对每个被释放块各产生一个 revoke，
因此 credits 按本次实际释放的数据块数逐一追加 revoke credit（与 Linux
get_default_free_blocks_flags() 对 S_ISDIR/S_ISLNK 的 METADATA|FORGET 语义一致）；regular
file 数据块不带该开销。计算结果仍为 allocator entry check 保留固定 headroom，并在任何
metadata mutation 前完成。

Ordered writeback 的 insert 与 unwritten conversion 只使用 `ExtentPath`，因此 transaction 内不再
切换到复杂度取决于整棵树大小的重建算法。它的 credits 在打开 transaction 前按本次 logical
block 数、extent 最大深度和每层 split 可能涉及的现有/new metadata targets 计算，不扫描已有
extent，也不再用 512 截断所需预算。跨叶 range removal 的全树回退仍使用 truncate planner 按
实际 tree blocks、revoke targets 和 affected groups 单独估算。

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

Bridge 在 mount 时缓存 filesystem block size 以及 extent/legacy 两个文件系统级上限；
bridge inode 通过其组合持有的 ext4 private state 查询 extent-format 状态。write、truncate、FIEMAP
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
区间 API，由一次持有 mount mutation guard 的操作同时更新 inode 区间、per-inode count 和
mount aggregate；bridge 不读取或调整任一计数。该 counter 与 group descriptor 由同一
allocation/release mutation 更新，因此 admission 是常数时间；显式 `statfs()` 仍遍历 group
descriptor，提供独立的实时统计与一致性观察面。

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
磁盘 hole 的 `BlockMappingFlags::DELAYED`。Bridge 只遍历统一 mapping 结果：普通 hole 被跳过，
mapped extent 输出物理块范围，unwritten 添加 `UNWRITTEN`，delayed hole 添加
`DELALLOC | UNKNOWN`。Legacy pointer 映射按连续 logical/physical block 合并并添加
`BlockMappingFlags::MERGED`，bridge 将其转换为 `FIEMAP_EXTENT_MERGED`。遍历保留一个
pending extent，只有确认查询范围内没有后续映射时才添加 `LAST`；输出容量满时立即停止且
不误标 `LAST`。

## 并发模型

运行态 mutation 当前通过 bridge 挂载级 `RwLock` 的 write guard 粗粒度串行化，只读调用可
共享 read guard。KVFS inode cache mutex 只保护 `New/Live/Freeing` identity state；每个 cache
slot 使用自己的等待队列，等待发生在释放 cache mutex 之后，因此一个 inode 的状态变化不会
唤醒其它 inode 的 `iget`。ext4 private state 使用独立 sleepable mutex，普通 metadata 读取或
mutation 不进入另一张 inode cache。
核心内部的 metadata buffer
和 JBD2 transaction handle 仍会记录 buffer ownership、credit consumption 和 revoke 状态。
同一 inode 的 `writepages()` 由 bridge 的 sleepable writeback mutex 串行化，但进入 PageCache
遍历时不持有挂载级 core mutex；PageCache 在释放 mapping/folio mutex 后调用 batch writer，
batch writer 才短暂取得 core mutex，并由 core 在该 guard 内更新 delalloc accounting。这样
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
- KExt4 的新生命周期与 I/O 语义只在 KExt4 core/bridge 落地，不保留第二套 ext4 实现路径。
- KExt4 core 只提供 inode 格式相关的最大文件大小和带 mapping flags 的 `BlockMapping`；Linux FIEMAP ABI、
  用户指针与输出容量留在 POSIX/KVFS 边界，bridge 只负责把 mapping 语义转换为 extent。
- `Ext4Filesystem` 保留类似 Linux `ext4_sb_info` 的 mount 总状态；只有具有独立事务状态机和
  生命周期不变量的 journal 聚合为 `MountedJournal`，不为代码分组机械创建 service。
- KExt4 只通过通用 `BlockDeviceOperations` 表达块读写和 flush；异步 request、完成通知和 VirtIO
  中断队列属于 block/driver 层。KExt4 可合并请求并在通用接口可用后接入，但不建立私有驱动
  旁路。
- errseq、clean unmount/freeze 和完整 fault matrix 依赖最终的后台执行图，集中放在 N3；它们
  不阻塞 N2 主路径，但仍是替换旧后端前的强制门槛。

## Drop / 资源释放

已分配的 metadata/data blocks 通过 journaled bitmap helper 释放。Inode 删除路径先切断
目录可达性，用 legacy orphan list 保护 zero-link cleanup；若 inode 带 external xattr
block，则先释放或降低 refcount，并清理 `i_file_acl`/`i_blocks`，然后 truncate
extent-backed data，清理 inode metadata，最后释放 inode bitmap entry。

运行态 bridge 仅在最后一个 writable-file `release()` 且没有 delayed data
reservation 时丢弃 EOF 后未使用的预分配，对应 Linux `ext4_release_file()`；
close 不额外强制普通 dirty PageCache writeback，数据回写由 `fsync`/`syncfs` 和通用
writeback 路径负责。`VfsInode` 最后一个引用消失时，superblock hook 先丢弃
PageCache/剩余 delalloc accounting，再对 nlink=0 inode 组合持有的 ext4 private state 调用
final eviction。KVFS 在 hook 前完成 `Live -> Freeing`，所以 cleanup 期间没有普通 VFS 能力；
三阶段 core API 始终借用该 `VfsInode` 组合持有的同一 private component；它不返回可逃逸的
eviction token 或另一种 inode handle。完成后 KVFS 精确删除旧 cache entry 并唤醒同号 `iget`。
nlink 非零的 cache eviction 不释放磁盘 inode；后续 `iget` 可从仍分配的磁盘 inode构造新的
private component。

Recovery 不创建 resident inode 表。它在 mount/VFS identity 尚未发布时通过 orphan-aware decode
取得临时 private state，复用正常 truncate 或 zero-link eviction。若 cleanup 失败，恢复证据
保留并阻止该 filesystem 被当作成功挂载。
