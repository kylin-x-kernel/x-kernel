# KExt4 实施路线图

本文是 KExt4 唯一的执行计划。它只维护当前事实、目标架构、阶段依赖和验收门槛；稳定的
架构与安全不变量分别维护在 `design.md` 和 `security.md`，公共 API 契约放在 rustdoc。

最后审计日期：2026-07-31。

当前基线：KExt4 N1.5 + N2.0 + N2.1。S0/S1 已恢复 KExt4 与重构后 KVFS 的连接，并建立 inode identity、
truncate、open-unlink 和 final eviction 基线；N1 已建立 persistent journal transaction、
commit/checkpoint 分离、per-inode sync/datasync transaction cursor 的正确所有权边界，以及与 Linux
`ext4_sb_info -> journal_t` 对应的 mount/journal 生命周期边界；N1.5 已收口 metadata mutation
失败语义；N2.0 已建立 dirty-folio index；N2.1 已在同步 block-device 约束内完成 journal
write batching、superblock image 复用和重复 flush 消除。
2026-07-13 形成但未提交的旧 S2 reservation/errseq/PageCache 原型已单独保存，不属于本
路线图的实现基线；后续只按新架构选择性复用其中的状态机和接口经验。

## 总目标

KExt4 是 X-Kernel 唯一的 ext4 后端。长期目标仍是完善遵循 Linux ext4/JBD2 核心架构、
可恢复且高效的 Rust 实现；开发顺序以结构性风险和主 workload 为中心，而不是按 syscall
数量或功能表逐项补齐。

路线图设置以下门槛：

| 门槛 | 目标 | 判定标准 |
| --- | --- | --- |
| G0：可运行基线 | KExt4 可编译、挂载并完成基本文件 I/O | 已完成；S0/S1 的 build、live 和 inode lifecycle 基线保留 |
| G1：核心框架成形 | mount ownership、persistent journal 和 transaction/checkpoint 分层稳定 | 普通 mutation 不再每次创建独立 journal 并同步 checkpoint；显式 sync 可推进全部状态 |
| G2：buffered fio 可用 | normal-path buffered I/O 和并发执行框架打通 | 固定 seq/rand/randrw/fsync smoke verify 通过；性能只记录，不先设阈值 |
| G3：可靠性闭环 | 异步错误、lifecycle、ENOSPC、crash/recovery 可验证 | errseq、clean unmount/freeze、fault/powercut/e2fsck 矩阵通过 |
| G4：发布收口 | 高级 I/O、常用格式和持续性能达到发布要求 | syscall/fio/fsstress/互操作/恢复矩阵持续通过，完成单后端发布说明 |

`direct=1`、mmap/shared-write 和 range operations 不属于 G2。buffered fio 通过不能被用来
宣称 direct I/O 或完整 Linux ext4 语义已经实现。

## 路线原则

### 按依赖推进，而不是按完备性阻塞

一个阶段只阻塞真正依赖它的工作。VFS/MM 公共接口可以由其他负责人并行实现；KExt4 core
不应因为 errseq、mount lifecycle 或 PageCache 的最终接口尚未合入而停止 persistent journal
和 ownership 重构。

### 先确定框架形状，再集中加固边界

会决定数据结构、ownership 和状态机的能力必须优先：mount state、persistent journal、
running/committing/checkpointing transaction、ordered-data dependency、per-inode range state
和锁域。完整 errseq、clean unmount、powercut/e2fsck 矩阵在真实的后台执行图形成后集中实现，
避免为即将替换的同步模型编写大量一次性代码。

### 每个框架切片保持可运行

“框架优先”不等于一次性重写。每个切片必须保持 mount、普通 mutation、显式 sync 和 remount
中的最小纵向路径可解释，并保留少量能保护状态机的 sentinel tests。完整边缘矩阵可以后移，
但不能以 panic、未定义 ownership 或静默写坏磁盘换取开发速度。

### 性能结论必须来自固定 workload

结构性瓶颈可以从代码确认并优先消除；具体优化和阈值必须使用相同平台、SMP、镜像、fio
版本和 job 参数比较 baseline/after。每次只改变一个主要变量，报告 observation、inference、
confidence 和 method limits。对比使用固定的 KExt4 历史基线，不保留旧后端作为可执行基线。

## 不可破坏的 ownership 边界

- `fs/filesystems/kext4` 拥有 ext4 磁盘格式、校验、JBD2、metadata buffer、extent、allocator、
  orphan、xattr 和持久化不变量；core 不依赖 KVFS 对象。
- KVFS 拥有 dentry、inode identity、open file、AddressSpace（含私有 `PageCache` folio
  storage）、mmap view 和 mount tree 生命周期。
- `fs/bridges/kext4_vfs` 只做语义适配、对象绑定、缓存属性同步和错误转换；bridge 不建立
  第二套 dentry cache、普通文件数据 cache 或 journal coordinator。
- 普通文件数据只通过 inode-owned AddressSpace/PageCache；ext4 元数据只通过 KExt4
  metadata buffer 和 JBD2 transaction 修改。
- KExt4 是 ext4 行为和接口的唯一实现，不保留旧后端兼容路径。
- 不通过关闭 journal、checksum、barrier/flush、跳过 e2fsck 或扩大一个全局锁来伪造结果。
- 重构、行为和性能提交分开；公共层修改必须表达通用 VFS/MM 契约，而不是 KExt4 旁路。

## 当前代码事实

### 可复用的 storage core

当前 `fs/filesystems/kext4` 是 `#![forbid(unsafe_code)]` 的 checked storage core，已有：

- checked superblock/group/inode/dirent/extent/xattr 解码、feature negotiation、system zone
  和 metadata checksum；
- JBD2 descriptor/revoke/commit/replay、csum_v2/csum_v3、32/64-bit tag 和显式 recovery；
- metadata buffer create/write/forget/revoke、frozen checkpoint snapshot、失败保留和
  reclaim 基线；
- extent lookup、unwritten mapping、insert/merge/remove/truncate、indexed-tree rebuild 和
  extent checksum；
- journaled block/inode bitmap、连续 run allocation、order-bucket free-run cache、partial
  allocation 和 simplified Orlov inode-group selection；
- sparse read、ordered-data writeback、writeback-time allocation、unwritten conversion、有限
  preallocation、truncate 和 legacy orphan；
- linear/HTree lookup、create/mkdir/link/unlink/rmdir/rename、symlink、special inode 和
  inode-body/single-external-block xattr baseline。

这些能力不应因 VFS 接口变化而重写，但当前执行仍被同步块 I/O 和挂载级锁过度串行化。

### 已完成的运行态基线

- S0：适配 `LockedDentry`、typed flags、最新 inode constructors、statfs 和 max-file-size；
- S1：一个 ext4 inode number 对应一个 live `VfsInode`/AddressSpace；
- S1：core namespace removal 与 final inode eviction 分离；
- S1：truncate 使用 core prepare → `AddressSpace` i_size/unmap/cache/unmap transaction → core finish；
- S1：legacy orphan recovery、open-unlink、hard-link identity 和 rename-overwrite identity 已有
  live 基线。

这些工作已经塑造正确的 VFS/core ownership，不再继续扩展为 syscall 完备性阶段。

### 当前结构性瓶颈

| 瓶颈 | 当前实现 | 为什么必须先改 |
| --- | --- | --- |
| mount-wide 串行化 | bridge 用一个 `Mutex<kext4::Ext4Filesystem>` 包住所有 core 调用 | journal、allocator、不同 inode 和只读路径无法并行 |
| commit batching 仍为同步驱动 | 普通 mutation 可共享 mount-wide running transaction；真实 outstanding credits、日志空间和显式 sync 可触发 commit | 尚无基于时间的 age trigger 和后台 commit worker，低负载 transaction 仍依赖后续操作或显式 sync 推进 |
| checkpoint 仍由调用者驱动 | mutation 返回后可保留 pending checkpoint，`syncfs`/unmount 或 journal-space pressure 同步推进 | 没有后台 writeback/checkpoint，home-block 脏状态和尾部回收仍会阻塞触发者 |
| journal 提交仍受同步 block API 限制 | N2.1 已把连续 journal blocks 聚合为不超过 128 KiB 的请求，并复用 journal-superblock image、去除 commit 后的重复 flush | 请求数量已降低，但每个 batch 仍在调用栈中同步完成；没有多请求 in-flight、后台 commit/checkpoint 或驱动 completion |
| durability 等待仍为同步基线 | runtime inode 尚无 sync/datasync tid，`fsync/fdatasync` 保守提交整个 running transaction 并 flush；`syncfs` 仍推进全文件系统 | 尚无目标 transaction 等待、异步 ordered-data dependency、后台 commit/checkpoint 和 errseq，等待者仍在当前调用栈执行设备 I/O |
| PageCache writeback 基础有限 | 固定 batch copy、无后台 dirty control、无 transaction dependency | fio 的吞吐和内存压力都不可控 |
| block/driver 接口只有同步完成路径 | KExt4 只能通过当前 `BlockDevice` 接口逐请求等待，VirtIO 完成通知和多请求 in-flight 不属于 filesystem 层 | KExt4 可以减少和聚合请求，但无法在 filesystem 内消除驱动 busy-poll/同步等待；需由 block/VirtIO owner 提供通用异步接口 |
| extent/allocator 仍未完整分层 | `ExtentPath` 已承载单路径更新和均衡 split，但跨叶 truncate 回退、重复 lookup 和 scan-backed free-run cache 仍存在 | 长文件常规写不再全树重建，复杂范围操作和多 job 仍受限 |

完整 errseq、unmount 和 fault matrix 很重要，但它们不消除上述结构性瓶颈，也不应继续阻塞
这些瓶颈的重构。

## 目标架构

```text
POSIX / KVFS
    |
    +-- mount / dentry / VfsInode / VfsFile
    +-- inode-owned AddressSpace / private PageCache folio storage / mmap views
                         |
                         v
                  kext4_vfs::Inode
                  - inode number
                  - sync/datasync transaction ids
                  - delalloc / unwritten / written range state
                  - VFS attribute synchronization
                         |
                         v
                Ext4Filesystem (mount state)
                +-- validated geometry/features and device
                +-- metadata buffer cache
                +-- group descriptors / allocator state
                +-- MountedJournal
                |     +-- on-disk journal mapping/superblock
                |     +-- transaction state
                |     |     +-- one object: Running -> Committing -> Checkpoint -> Finished
                |     +-- FIFO checkpoint queue / runtime head-tail
                +-- inode / extent / orphan / xattr operations
                         |
                         v
                  block::BlockDevice
```

目标映射：

| Linux 对象/机制 | KExt4 目标所有者 |
| --- | --- |
| `super_block` / `ext4_sb_info` | KVFS `SuperBlock` + KExt4 `Ext4Filesystem` mount state；不为字段分组机械拆 service |
| `ext4_inode_info` | bridge/core per-inode state；`i_size/i_disksize`、sync tids、delalloc/PA/extent-status |
| `address_space` | KVFS `AddressSpace` 唯一拥有 identity/views；`PageCache` 是其私有 folio storage，KExt4 提供 address-space operations |
| `jbd2_journal` | mount-wide `MountedJournal`；聚合磁盘 journal、transaction engine 和 checkpoint queue |
| running/committing/checkpoint transaction | `MountedJournal`、checkpoint queue 和 metadata buffer 共同维护明确 ownership |
| extent status tree | per-inode logical-range cache，不替代磁盘 extent tree |
| `mb_group_info` / buddy / PA | per-group allocator state 和 inode/locality preallocation state |
| orphan list/orphan file | KExt4 core 持久化；VFS final eviction 与 recovery 触发 |

## 并行工作流与依赖

路线分成三条并行工作流：

1. **Core architecture**：mount state、journal、metadata buffer、allocator、extent 和
   per-inode state，由 KExt4 主线推进。
2. **Shared VFS/MM contracts**：AddressSpace cache/mmap invalidation、errseq 和 mount lifecycle，由对应
   负责人实现通用接口；KExt4 只提供需求和适配。
3. **Validation**：最小 sentinel、normal fio、fault/powercut 和长期 workload，按阶段逐级
   扩大，不把最终矩阵复制到每一个早期提交。

依赖关系：

```text
N0 baseline
    |
    v
N1 persistent journal / mount ownership (done) -+
    |                                            |
    v                                            |
N1.5 mutation failure / explicit unwind --------+
    |                                            |
    v                                            |
N2 buffered runtime / concurrency <--- VFS/MM PageCache + runtime-inode cursor contracts
    |
    v
N3 reliability / lifecycle <--------- VFS errseq + unmount/freeze contracts
    |
    v
N4 evidence-driven performance
    |
    v
N5 advanced I/O / common features -> N6 replacement
```

## 实施阶段

### N0：冻结可运行基线

状态：已完成。

目标：保留 S0/S1 已形成的 VFS/core ownership 和最小纵向路径，不再为了边界完备性延长旧
同步架构的生命周期。

退出事实：

- KExt4 配置可 build、clippy、mount 和完成基本 namespace/buffered I/O；
- inode identity、truncate 和 final eviction 的职责边界已经建立；
- 旧 S2 原型不作为基线提交，公共层需求单独移交。

### N1：建立 mount state 与 persistent journal 框架

状态：已完成（2026-07-21）。

目标：消除“每个 mutation 新建 journal 并同步 checkpoint”的核心结构问题，为后台执行和
细粒度并发建立稳定 ownership。N1 首先同步驱动状态机，不要求立即创建 worker。

实现项：

- `Ext4Filesystem` 作为类似 Linux `ext4_sb_info` 的 mount 总状态，持有 geometry/device、
  metadata cache、group/allocator state 和各类 ext4 operation；不按字段类别机械拆 service；
- `MountedJournal` 作为类似 JBD2 `journal_t` 的 mount-lifetime journal 边界，同时持有磁盘
  journal 映射/superblock、唯一 transaction engine 和 pending checkpoint queue；
- transaction engine 让同一个 transaction 对象明确经历 Running、Committing、Checkpoint 和
  Finished phase，并由 journal 独立表达 aborted 状态；
- transaction handle 按 credits 加入 running transaction，commit trigger 与 checkpoint
  progress 分离；
- metadata buffer 保留 running owner、committing frozen image 和 checkpoint image；
- 明确 per-inode sync/datasync transaction id 属于 VFS runtime inode，journal 不保存
  inode-number 全局 cursor；接口就绪前 `fsync` 保守提交整个 running transaction；
- 显式 `fsync/syncfs` 可以保守推进 inode/filesystem 所需的 transaction；
- 普通 mutation 返回前不再无条件完成 home-block checkpoint。

已完成的切片：

- mount 根据磁盘 journal sequence 创建并持续持有同一个 transaction engine，连续 mutation 不再
  重建内存 transaction 状态；
- 磁盘 journal 映射、transaction engine 和 FIFO checkpoint queue 已聚合进同一个
  `MountedJournal`；commit/checkpoint API 只能通过该 mount-owned identity 进入，pending queue
  只保存同一个 runtime transaction；持久化证据属于其 Checkpoint phase，不再复制 transaction
  payload、创建持有 `Arc` 的 checkpoint wrapper 或保存反向 journal 引用；
  allocator、metadata 和 device 继续由 `Ext4Filesystem` mount state 持有；
- allocator、extent、truncate/orphan、namei、xattr、inode metadata 和 ordered writeback
  已把预期失败前移到首次 metadata access 之前：先完成格式/存在性/容量/credits/extent path
  校验与资源预算，再进入 transaction 内的 byte publication；
- `write_access` 和 `create_access` 只建立 transaction membership 和 frozen checkpoint
  ownership，不记录 syscall/operation 或 running-transaction 级 undo image；access 前若已
  发布新的 metadata/revoke membership，后续失败必须永久 abort journal，不能由单个 handle
  局部撤销共享 membership，也不能回滚其他已成功返回的 operation；
- 未提交 transaction 取消后复用 sequence，避免 checkpoint 水位出现不可闭合空洞；完成
  checkpoint 的 commit 记录会被回收，并用支持 `u32` 回绕的连续水位表达历史状态；
- handle stop 归还未使用 credits；多个 handle 可以独立退出并继续共享 mount-wide running
  transaction。新 mutation 加入前按预计 outstanding credits 检查容量，普通 transaction 采用
  约三分之一日志容量的保守上界，不再使用固定 operation 数量；
- 磁盘日志可在运行态 append head 连续追加多个 committed、未 checkpoint transaction；
  active superblock 的 sequence/start 表达最老 live transaction，`s_head` 只在 journal clean
  时写入。clean journal 首次提交先持久化并 flush 非零 `s_start`，再写
  descriptor/data/commit，使该中间掉电窗口被恢复视为 active 但未完成的 transaction。追加按
  环形已用空间保护 tail，并保留一个空 block 防止覆盖 live descriptor；
- FIFO checkpoint 完成一个 transaction 后把 tail 推进到下一个，只有队列清空才把 journal
  标记 clean 并清除 ext4 `needs_recovery`；设置 recovery evidence 不会用旧 home-block
  superblock 覆盖更新的内存 allocator counters；primary superblock 的 journal image 与 frozen
  checkpoint image 都合并 recovery feature，旧 checkpoint 不会在新 commit pending 时清除此
  恢复证据；
- unit test 和真实 Linux ext4 镜像测试覆盖双 transaction scan、逐次 tail 推进、最终 clean
  与 e2fsck；
- replay 后 transaction engine 会原子重置到 report 的 next sequence，再运行 orphan cleanup；
  internal-journal extent 同时校验至少覆盖 JBD2 `s_maxlen` 的 logical range 和 filesystem
  device physical bounds，并允许 backing inode 的预分配容量大于 `s_maxlen`；
- 普通 mutation 在 handle 内完成成功/失败决定后即可返回；触发 commit 时只要求 journal
  commit durable，home-block checkpoint 保留在 FIFO queue；`syncfs` 和 KVFS unmount
  writeback 会先提交 running transaction 再 drain 全队列，普通 mount 的 dentry eviction
  后再同步一次；journal-space pressure 会推进最老 checkpoint 后重试 append；
- clean v2 journal 在首个 mutation 前持久化开启 revoke feature，再允许 transaction/checkpoint
  重叠；无 feature bitmap 的 v1 journal 保留每个 operation 同步 commit/checkpoint 退化路径；
- truncate 的 orphan + `i_disksize` 更新保留强制 commit 边界，确保后续释放旧映射前已有可
  恢复状态；recovery-time orphan cleanup 也保持同步 commit/checkpoint，不加入普通 batch。
  即使 JBD2 已 clean，只要 legacy orphan head 非零，首个 cleanup transaction 也会采用
  `PreserveDuringRecovery` 建立 recovery evidence；全部 cleanup 完成后确认 journal start 为零，
  再清除并 flush ext4 recovery feature；
- journal 不再保存 inode-number keyed sync/datasync cursor；bridge `fsync/fdatasync` 先完成
  目标 inode PageCache writeback，再保守提交整个 running transaction 并 flush。精准 target
  transaction 等待需由 runtime inode 保存 tid，并在 mutation 完成后发布；
- unit test 和真实 Linux ext4 镜像测试覆盖修改前预期错误、修改后 invariant abort、credit
  归还、多 handle 独立 stop、transaction-id wrap、FIFO checkpoint、replay 后 orphan cleanup，
  clean journal 上持久化的 legacy orphan、recovery flush 失败后的证据保留与重试，以及最终
  `syncfs` drain/recovery 后的 e2fsck。

移交 N2 的后续工作：

1. 当前已完成 credits、journal-space 和 explicit-sync commit trigger；基于时间的 age trigger
   随 background commit worker 引入，避免为同步驱动阶段提前增加 timer ownership；
2. 同步 ordered-data 基线已经完成；下一步需要 KVFS runtime inode 提供 sync/datasync tid，
   KExt4 再接入目标 transaction 等待，并在 N2 引入异步 ordered-data dependency、后台
   commit/checkpoint 和 errseq，使等待不再由 fsync 调用栈串行执行全部设备 I/O；
3. N1 保持同步调用，不提前引入 worker 和细粒度锁；N2 根据真实执行者分别为 journal、
   inode、metadata buffer 和 group allocator 建立锁域，不再以字段分组代替并发设计。

退出条件：

- 连续且不冲突的 metadata mutation 可加入同一个 running transaction；
- running transaction 冻结后可以开始新的 running transaction；
- commit 与 checkpoint 是两个可独立推进、可等待的阶段；
- 显式 filesystem sync 能同步驱动所有 pending state 到稳定存储；
- 最小 sentinel 覆盖未提交 transaction 不 replay、已提交 transaction 可 replay、ordered
  data 先于相关 metadata commit；
- 代码和文档明确：`Ext4Filesystem` 拥有 device、geometry、metadata buffer 和 allocator；
  `MountedJournal` 拥有 journal sequence、transaction engine 和 checkpoint queue。

### N1.5：收口 metadata mutation 失败语义

状态：已完成。N2.1 可以在此失败语义之上继续降低同步 journal I/O；N2.2 后台 journal
执行和 N2.4 多 handle 并发不得重新引入 operation-local transaction。

目标：删除当前 operation savepoint 这套过渡性 syscall/operation 级回滚系统，使 ext4
mutation 通过“修改前校验与资源预留 + path-local 显式 unwind”处理预期错误；JBD2 handle
最终只承担 credits 和 buffer/revoke membership，并由显式 stop 返回 accounting 错误；journal
独立承担 abort，不再保存 operation 或 running-transaction 级 metadata byte copies。

已完成项：

- 盘点 allocator/bitmap、extent、truncate/orphan、namei、xattr、inode metadata 和 ordered
  writeback 的所有失败点，区分可在首次 metadata 修改前发现的预期错误与必须 abort 的
  journal/invariant 错误；
- 把权限、格式、目标存在性、journal credits、block/inode 空间和 extent path 可表达性校验
  前移；跨叶或全树 fallback 必须在首次 metadata write/create/revoke 前选定；
- allocator 先在私有 bytes 和 free-extent cache 副本上完成 bitmap/counter 计算，再一次性取得
  journal access 并发布；extent、namei、truncate/orphan、xattr 和 ordered writeback 在 handle
  创建前完成容量、目标状态和 path 可表达性检查。当前同步、挂载级串行 mutation 没有跨外部
  执行者的资源取得阶段，因而不另建一套通用 unwind 栈；
- 允许多个 handle 独立 stop，不再依赖“失败 operation 必须是全 transaction 最新 savepoint”
  这一全局约束；后台 commit 不得观察到“handle 已关闭但 operation 尚未决定成功/失败”的中间态；
- 所有生产 mutation 完成显式 unwind 改造后，删除 `JournalOperationState`、operation token、
  savepoint map、operation-local undo bytes、全 running-transaction undo 和兼容性的
  latest-savepoint API；
- 为每类 mutation 增加 success、发布前预期错误、发布后 invariant abort 和前序成功 operation
  不被回滚的窄回归测试；复杂 namespace/truncate/extent 路径额外用真实镜像和 e2fsck 验证。

实现约束：预期错误只能在 handle 尚未发布 metadata 时原样返回。若普通错误在
`has_updates()` 之后出现，说明遗漏了修改前校验或 path-local unwind；实现把它升级为
`InvalidJournalTransaction` 并永久 abort journal，不回滚整个 running transaction。设备 I/O、
checksum、journal 状态机和 metadata invariant 错误同样直接 abort。transaction membership
发布后不允许单个 handle 在失败路径上局部删除，因为同一 metadata block 可能已被其他 handle
共享。已经成功返回的 operation 保持其内存修改；尚未发布的私有资源由具体 ext4 算法显式
unwind。磁盘一致性由 durable commit、recovery evidence 和 replay 保证，不能声称已完成
syscall 级局部回滚。

退出条件：

- 普通 `NoSpace`、`AlreadyExists`、`NotFound` 和不支持格式等预期错误不会因为缺少最新
  savepoint 而 abort journal；
- 多个 handle 可以在同一 running transaction 中独立加入和退出，任一预期 operation 失败
  不会回滚其他已成功 operation；
- 生产代码不再创建 operation savepoint，也不复制 operation-local 或全 transaction metadata
  block image；
- journal I/O、checksum、内部状态机、无法恢复的 metadata invariant，以及遗漏 preflight 后
  发生的发布后普通错误会 abort；
- namespace、truncate/orphan、xattr、extent/allocator 和 ordered-writeback 回归通过，
  注入失败后的镜像可重新 mount，e2fsck 不报告 metadata corruption。

### N2：建立 buffered runtime 与并发框架

状态：N2.0、N2.1 已完成；N2.2 及后续待实现。

目标：把 PageCache、delalloc、ordered transaction、后台执行和局部锁连成主数据路径，达到
normal-path buffered fio 可用，而不是先追求所有故障边界。

实现项：

- **N2.1：先降低同步前台 I/O 数量**：在现有同步 `BlockDevice` 能力内合并连续 journal
  blocks，复用已校验的内存 journal superblock，审计并去除一次 commit/checkpoint 内重复的
  read/write/flush；barrier 和最终持久化语义不得削弱；
- **N2.2：建立后台 journal 执行**：引入 background commit/checkpoint、transaction age
  trigger、journal-space watermark 和 wait-by-tid；多个并发等待者共享同一 transaction commit，
  checkpoint 由后台或 space/syncfs/unmount 推进；
- **N2.2-fsync：接入精准 inode sync cursor**：KVFS runtime inode 拥有 `sync_tid` 和
  `datasync_tid`，KExt4 mutation 成功后发布目标 transaction id；`fsync/fdatasync` 在完成目标
  inode PageCache writeback 后只请求并等待对应 tid durable，不提交该 inode 已 durable 之后由
  其他 inode 创建的无关 running transaction。inode eviction/reuse 必须清理 cursor，commit
  error 继续交给 N3 errseq/forced-readonly 观察面；
- **N2.3：建立 ordered writeback 与 range ownership**：per-inode logical-range state 表达
  hole/delayed/unwritten/written、dirty 和 writeback；reservation/allocator accounting 使用
  range ownership，writeback batch 关联 ordered-data dependency 和目标 transaction；
- **N2.4：按执行者拆锁**：去除 bridge 的 mount-wide core mutex，建立 per-inode、journal、
  metadata-buffer 和 per-group allocator 锁域；不为尚未存在的 worker 预拆字段 service；
- 在已有 path-local find/insert/split 基础上补齐跨叶 remove/truncate，消除重复 lookup 与剩余
  全树回退；allocator 建立稳定的 group/buddy/preallocation ownership；
- 保持 writeback、fdatasync、fsync 和 syncfs 不同 durability intent；当 block 层提供通用异步
  request/completion 接口后，KExt4 只负责形成和提交批次，不在 filesystem 内实现 VirtIO
  completion 或 busy-poll 替代逻辑。

N2.1 已完成项：

- commit encoder 把 descriptor、data、revoke 和 commit blocks 形成不超过 128 KiB 的
  journal write batch，并在 journal ring wrap 和 internal-journal physical extent 边界拆分；
- `JournalSuperblock` 保存挂载时已校验的完整 block image，sequence/start/feature 更新不再
  重读 journal block 0；仅读取 feature/credit limit 的热路径也不复制完整 image；
- 保留 clean-journal activation flush 和 transaction 最终 durability flush；inode sync 在
  metadata commit 已提供最终 barrier 时不再紧接一次重复 flush，没有 metadata commit 时仍
  显式 flush；
- sentinel tests 记录 write-request block count、superblock read count、ring-wrap 拆分和
  128 KiB 上界；批量测试后端仍按 logical block 保留失败注入语义。

G2 退出条件：

- fixed normal-path buffered fio：seq write/read verify、rand write/read、2/4-job randrw 和
  fsync smoke 可重复通过；
- 多 inode I/O 不再被一个 mount-wide mutex 全部串行；
- `fsync/fdatasync` 能按 runtime inode 的 sync/datasync tid 等待目标 commit，重复同步已
  durable inode 不触发无关 transaction commit；
- 普通 mutation 不再强制 home-block checkpoint；
- 记录 commit/checkpoint/flush 次数、dirty/reservation 峰值和基础 fio 指标；
- 本阶段不以完整 fault、powercut、errseq 或 clean unmount matrix 作为退出条件。

### N3：可靠性、错误观察与 lifecycle 加固

目标：在最终异步执行图上补齐失败语义，而不是加固已经被替换的同步模型。

实现项：

- AddressSpace writeback error、SuperBlock filesystem error 和 per-file errseq sample/cursor；
- journal/checkpoint abort 后的 forced-readonly 和新写入 gate；
- KVFS clean unmount/freeze：阻止新引用或写入，drain PageCache、inode metadata、running
  transaction、commit/checkpoint worker 和 orphan，再 detach/freeze；当前同步 unmount
  已保证 writeback 失败不拆 topology，仍需补引用 gate、freeze 和后台 worker 协调；
- short copy、redirty-during-writeback、invalidate/reclaim/truncate、ENOSPC 和 retry；
- block read/write/flush、journal persist、checkpoint、bitmap/counter、extent split 和 orphan
  故障注入；
- powercut、recovery、Linux mount/debugfs/e2fsck 互操作矩阵。

G3 退出条件：

- 后台 writeback/checkpoint error 可由相关 `fsync/syncfs` 观察；
- clean unmount/remount 数据一致，失败的 unmount 不错误拆除 mount tree；
- crash 后只能恢复为合法旧状态或新状态；
- fault/powercut 后 KExt4 和 e2fsck 都不报告静默 metadata corruption；
- N2 buffered fio smoke 在启用可靠性基础设施后仍通过。

### N4：基于证据的性能收口

目标：只优化固定 workload 已定位的瓶颈，达到项目记录的可用性能门槛。

候选项不是预先承诺的实现清单，只有证据命中后才进入提交：

- extent-status hit rate、mapping lookup 和 readahead；
- mballoc criteria、buddy、locality/preallocation 命中率；
- transaction age、credit estimate、commit batching、journal tail reclaim；
- folio/segment I/O 聚合、临时复制和 allocation；
- per-inode/per-group/journal/metadata-buffer lock contention。

每个优化必须固定平台、SMP、镜像、fio 版本和 job 参数，报告 baseline/after、delta 和方法
限制。只有 workload 指向同步争用时才使用 lock_stat，不以 acquisition 次数代替 contention
证据。

退出条件：

- buffered fio 和 metadata workload 达到项目在首轮稳定基线后记录的阈值；
- 性能提升不能关闭 journal/checksum/barrier，也不能破坏 N3 gates；
- 性能对比只使用固定 workload 形成的 KExt4 历史基线。

### N5：高级 I/O 与常用功能

目标：在稳定的 buffered/journal/lifecycle 架构上补齐常见 Linux I/O 和 metadata 语义。

- shared writable mmap、write fault、msync 和 truncate coherence；
- direct I/O、buffered/direct overlap 和 ordered-data completion；
- fallocate、preallocation、punch-hole 及其他按 workload 需要的 range operations；
- xattr syscall surface、ACL permission/inheritance、security namespace policy；
- HTree write scalability、large directory 和常见 ext4 feature；
- 每项格式能力必须同时考虑 feature negotiation、checksum、journal credits、recovery 和
  Linux/e2fsprogs 互操作。

### N6：唯一后端收口

目标：在移除旧后端后，以持续回归和迁移验证保证 KExt4 单线演进。

退出条件：

- 常见 rootfs、package/build、fio、fsstress 和目标应用长期稳定；
- syscall、format、fault/recovery 和 performance matrix 进入持续 CI；
- rootfs 配置、文档和持续集成只引用 KExt4，并保留镜像兼容说明；
- 达到 G4 后把 KExt4 的长期性能和可靠性门槛纳入发布要求。

## 交给 VFS/MM 负责人的公共需求

这些需求由共享层负责人推进，可与 KExt4 的 N2/N3 工作并行，不要求采用旧 S2 原型的具体
实现：

1. **PageCache invalidation contract**：truncate、ordinary invalidation、reclaim 和 final
   teardown 可通知 filesystem address-space operation；明确 mapping/folio 锁上下文，callback
   不在 tree lock 下执行 backing I/O。
2. **Buffered prepare cancellation**：`write_begin()` 成功后所有退出路径与一次
   `write_end()` 配对；PageCache grow/copy 失败使用 `copied == 0`。
3. **Runtime inode journal cursor**：KVFS runtime inode 提供 `sync_tid`/`datasync_tid`
   存储、更新和 eviction/reuse 清理接口；KExt4 bridge 只在 core mutation 成功后发布 tid，
   不以 inode number 为 key 在 mount journal 中复制 cursor 所有权。
4. **Writeback errseq**：mapping error 向 filesystem-wide error 聚合；open file 分别采样
   file/superblock cursor；`fsync/syncfs` 按 Linux 语义检查并推进。
5. **Filesystem lifecycle**：unmount/freeze 能 gate 新引用/写入，drain VFS-owned data 和
   filesystem hook，只有成功后才 detach/标记 frozen，失败可以恢复 active state。

KExt4 bridge 只消费这些通用接口，不为 KExt4 建立私有 VFS/PageCache 旁路，也不保留
旧后端兼容路径。

## 交给 block/VirtIO 负责人的外部依赖

以下能力不属于 ext4/filesystem 实现范围，应建立独立 issue 由 block/driver owner 推进：

1. `BlockDevice` 支持异步 request submission 和 completion notification，而不是要求调用者
   对每个请求同步等待或 busy-poll；
2. 支持多个 read/write request 同时 in-flight，并保留完成顺序、错误和取消语义；
3. VirtIO-blk 使用中断/完成队列唤醒等待者，并向通用 block 层暴露能力，不建立 KExt4 私有
   VirtIO 路径；
4. 明确 cache flush、barrier、FUA 和设备错误的通用契约，保证 filesystem 可以表达 journal
   commit 的持久化边界。

该依赖不阻塞 N2.1 的请求合并、flush 精简，也不阻塞先用内核任务运行同步
commit/checkpoint worker；但在通用异步接口落地前，KExt4 无法像 Linux bio/JBD2 那样并行
提交多笔块 I/O，单请求等待和驱动轮询仍会形成性能下限。KExt4 侧只记录需求、准备批量 I/O
边界，并在接口可用后接入。

## 分层验证策略

### L0：每个代码切片

- KExt4 配置的 format/build/clippy；
- 触及状态机的窄 unit tests；
- diff review：ownership、sleepability、锁顺序、错误传播和文档同步。

### L1：框架 sentinel

- mount、一个普通 mutation、显式 sync、remount；
- committed/uncommitted journal replay；
- ordered-data 的最小可观察顺序；
- 一个 operation 在 metadata 发布前预期失败后，前序成功 operation 保留且当前 transaction
  仍可推进；发布后故障则验证 journal abort 且前序成功 operation 不被内存回滚；
- 不扩大为每个 syscall 的 fault matrix。

### L2：normal workload

- 固定 buffered fio smoke 和少量 metadata workload；
- targeted `fsync/fdatasync` 验证只等待 runtime inode cursor 指向的 transaction，重复同步不
  触发无关 commit；
- 记录而非预设性能阈值；
- 用于 N2 主路径和 N4 baseline/after。

### L3：reliability matrix

- ENOSPC、writeback/checkpoint fault、powercut、recovery、e2fsck；
- 只在 N3 最终执行图上系统展开。

### L4：replacement matrix

- syscall/xfstests、direct/mmap/range、fio/fsstress、跨格式与长期 soak；
- 用于 N5/N6，不复制为 N1 的前置条件。

所有 Make/QEMU 验证先从目标 defconfig 准备 `.config`。不能用 bare
`cargo check -p kext4` 代替 Kconfig/Makefile 流程。

## 当前下一步

N1 已满足其同步框架退出条件；N2.0 dirty-folio index 由独立分支维护，不包含在本 N1 分支。
后续不继续旧 S2，也不提前展开 N3：

1. N1.5 已按 allocator/extent → truncate/orphan → namei/xattr → ordered writeback 的顺序
   完成修改前校验收口，并删除 operation savepoint/token、local undo 和全 transaction undo；
   后续阶段保持这一边界；
2. 完成 N2.1，在现有同步 block API 内合并 journal block 请求、减少重复 journal superblock
   I/O 和 flush，并用固定 `fio-powercut` 记录请求次数与 runtime；
3. 推动 KVFS runtime inode cursor 接口，并在接口可用后让 KExt4 mutation 发布
   sync/datasync tid；在 wait-by-tid 闭环完成前保留“提交整个 running transaction”的正确性
   fallback，但不得宣称精准 inode sync 已完成；
4. 完成 N2.2 后台 commit/checkpoint、age/space trigger 和 wait-by-tid，同时关闭
   N2.2-fsync，使 `fsync/fdatasync` 只等待目标 commit；随后推进 N2.3/N2.4 的 ordered range
   state、writeback dependency 和按执行者拆锁；
5. 同时向 block/VirtIO owner 提交通用异步 request/completion issue；KExt4 不直接修改驱动，
   接口可用后再接入多请求 in-flight。

旧 `refactor-spec.md` 中仍有价值的 metadata buffer/JBD2、path-local extent、mballoc、
delalloc、background checkpoint、errseq、direct-I/O 和 fault matrix 内容已按上述依赖重新
归入 N1-N5；旧 R0-R11 和当前废弃的 S2-S8 顺序不再继续维护。
