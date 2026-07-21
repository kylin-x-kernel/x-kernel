# KExt4 实施路线图

本文是 KExt4 唯一的执行计划。它只维护当前事实、目标架构、阶段依赖和验收门槛；稳定的
架构与安全不变量分别维护在 `design.md` 和 `security.md`，公共 API 契约放在 rustdoc。

最后审计日期：2026-07-13。

当前基线：commit `d8fb2a93`。S0/S1 已恢复 KExt4 与重构后 KVFS 的连接，并建立 inode
identity、truncate、open-unlink 和 final eviction 基线。2026-07-13 形成但未提交的旧 S2
reservation/errseq/PageCache 原型已单独保存，不属于本路线图的实现基线；后续只按新架构
选择性复用其中的状态机和接口经验。

## 总目标

KExt4 的长期目标是实现一个遵循 Linux ext4/JBD2 核心架构、可恢复且高效的 Rust ext4
后端，并最终替代 rsext4。开发顺序以结构性风险和主 workload 为中心，而不是按 syscall
数量或功能表逐项补齐。

路线图设置以下门槛：

| 门槛 | 目标 | 判定标准 |
| --- | --- | --- |
| G0：可运行基线 | KExt4 可编译、挂载并完成基本文件 I/O | 已完成；S0/S1 的 build、live 和 inode lifecycle 基线保留 |
| G1：核心框架成形 | mount ownership、persistent journal 和 transaction/checkpoint 分层稳定 | 普通 mutation 不再每次创建独立 journal 并同步 checkpoint；显式 sync 可推进全部状态 |
| G2：buffered fio 可用 | normal-path buffered I/O 和并发执行框架打通 | 固定 seq/rand/randrw/fsync smoke verify 通过；性能只记录，不先设阈值 |
| G3：可靠性闭环 | 异步错误、lifecycle、ENOSPC、crash/recovery 可验证 | errseq、clean unmount/freeze、fault/powercut/e2fsck 矩阵通过 |
| G4：默认后端 | 高级 I/O、常用格式和持续性能达到替换要求 | syscall/fio/fsstress/互操作/恢复矩阵持续通过，完成替换说明 |

`direct=1`、mmap/shared-write 和 range operations 不属于 G2。buffered fio 通过不能被用来
宣称 direct I/O 或完整 Linux ext4 语义已经实现。

## 路线原则

### 按依赖推进，而不是按完备性阻塞

一个阶段只阻塞真正依赖它的工作。VFS/MM 公共接口可以由其他负责人并行实现；KExt4 core
不应因为 errseq、mount lifecycle 或 PageCache 的最终接口尚未合入而停止 persistent journal
和 ownership 重构。

### 先确定框架形状，再集中加固边界

会决定数据结构、ownership 和状态机的能力必须优先：mount services、persistent journal、
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
confidence 和 method limits。rsext4 只作为只读对比基线，不再为其扩展或修复新功能。

## 不可破坏的 ownership 边界

- `fs/filesystems/kext4` 拥有 ext4 磁盘格式、校验、JBD2、metadata buffer、extent、allocator、
  orphan、xattr 和持久化不变量；core 不依赖 KVFS 对象。
- KVFS 拥有 dentry、inode identity、open file、AddressSpace、PageCache、mmap view 和 mount
  tree 生命周期。
- `fs/bridges/kext4_vfs` 只做语义适配、对象绑定、缓存属性同步和错误转换；bridge 不建立
  第二套 dentry cache、普通文件数据 cache 或 journal coordinator。
- 普通文件数据只通过 inode-owned AddressSpace/PageCache；ext4 元数据只通过 KExt4
  metadata buffer 和 JBD2 transaction 修改。
- KExt4 的新接口和行为不要求同步修复 rsext4；最终目标是替换它。
- 不通过关闭 journal、checksum、barrier/flush、跳过 e2fsck 或扩大一个全局锁来伪造结果。
- 重构、行为和性能提交分开；公共层修改必须表达通用 VFS/MM 契约，而不是 KExt4 旁路。

## 当前代码事实

### 可复用的 storage core

当前 `fs/filesystems/kext4` 是 `#![forbid(unsafe_code)]` 的 checked storage core，已有：

- checked superblock/group/inode/dirent/extent/xattr 解码、feature negotiation、system zone
  和 metadata checksum；
- JBD2 descriptor/revoke/commit/replay、csum_v2/csum_v3、32/64-bit tag 和显式 recovery；
- metadata buffer create/write/undo/forget/revoke、frozen checkpoint snapshot、失败保留和
  reclaim 基线；
- extent lookup、unwritten mapping、insert/merge/remove/truncate、indexed-tree rebuild 和
  extent checksum；
- journaled block/inode bitmap、连续 run allocation、order-bucket free-run cache、partial
  allocation 和 simplified Orlov inode-group selection；
- sparse read、ordered-data writeback、writeback-time allocation、unwritten conversion、有限
  preallocation、truncate 和 legacy orphan；
- linear/HTree lookup、create/mkdir/link/unlink/rmdir/rename、symlink、special inode 和
  inode-body/single-external-block xattr baseline。

这些能力不应因 VFS 接口变化而重写，但它们当前仍被一个过度同步的 mount aggregate 驱动。

### 已完成的运行态基线

- S0：适配 `LockedDentry`、typed flags、最新 inode constructors、statfs 和 max-file-size；
- S1：一个 ext4 inode number 对应一个 live `VfsInode`/AddressSpace；
- S1：core namespace removal 与 final inode eviction 分离；
- S1：truncate 使用 core prepare → PageCache/mmap resize → core finish；
- S1：legacy orphan recovery、open-unlink、hard-link identity 和 rename-overwrite identity 已有
  live 基线。

这些工作已经塑造正确的 VFS/core ownership，不再继续扩展为 syscall 完备性阶段。

### 当前结构性瓶颈

| 瓶颈 | 当前实现 | 为什么必须先改 |
| --- | --- | --- |
| mount-wide 串行化 | bridge 用一个 `Mutex<kext4::Ext4Filesystem>` 包住所有 core 调用 | journal、allocator、不同 inode 和只读路径无法并行 |
| journal coordinator 非持久 | `metadata_journal()` 每次按当前 sequence 新建 `Journal` | handle 无法加入同一个 running transaction |
| mutation 前后同步 drain | 新 transaction 前 drain 旧 checkpoint，commit 后立即运行 checkpoint | 小写、create/unlink/rename 延迟被 journal/home-write/flush 放大 |
| ownership 聚合 | geometry、device、journal、metadata cache、allocator 和 inode mutation 都由一个可变 aggregate 驱动 | 无法为后台 worker 和细粒度锁建立稳定生命周期 |
| durability 边界粗糙 | writeback、fdatasync、fsync、syncfs、checkpoint 常落到同类 flush | 无法按 transaction 和 inode 精确等待 |
| PageCache writeback 基础有限 | 固定 batch copy、无后台 dirty control、无 transaction dependency | fio 的吞吐和内存压力都不可控 |
| extent/allocator 仍未完整分层 | `ExtentPath` 已承载单路径更新和均衡 split，但跨叶 truncate 回退、重复 lookup 和 scan-backed free-run cache 仍存在 | 长文件常规写不再全树重建，复杂范围操作和多 job 仍受限 |

完整 errseq、unmount 和 fault matrix 很重要，但它们不消除上述结构性瓶颈，也不应继续阻塞
这些瓶颈的重构。

## 目标架构

```text
POSIX / KVFS
    |
    +-- mount / dentry / VfsInode / VfsFile
    +-- inode-owned AddressSpace / PageCache / mmap views
                         |
                         v
                  kext4_vfs::Inode
                  - inode number
                  - sync/datasync transaction ids
                  - delalloc / unwritten / written range state
                  - VFS attribute synchronization
                         |
                         v
                KExt4 mount services
                +-- immutable validated geometry/features
                +-- filesystem device and flush boundary
                +-- persistent JournalCoordinator
                |     +-- running transaction
                |     +-- committing transaction
                |     +-- checkpoint queue / journal tail
                +-- metadata buffer cache
                +-- per-group allocator state
                +-- inode / extent / orphan / xattr operations
                         |
                         v
                  block::BlockDevice
```

目标映射：

| Linux 对象/机制 | KExt4 目标所有者 |
| --- | --- |
| `super_block` / `ext4_sb_info` | KVFS `SuperBlock` + KExt4 mount services；geometry 与可变 service state 分离 |
| `ext4_inode_info` | bridge/core per-inode state；`i_size/i_disksize`、sync tids、delalloc/PA/extent-status |
| `address_space` | KVFS/PageCache 唯一拥有；KExt4 提供 address-space operations |
| `jbd2_journal` | mount-wide persistent `JournalCoordinator` |
| running/committing/checkpoint transaction | coordinator 和 metadata buffer 共同维护明确 ownership |
| extent status tree | per-inode logical-range cache，不替代磁盘 extent tree |
| `mb_group_info` / buddy / PA | per-group allocator state 和 inode/locality preallocation state |
| orphan list/orphan file | KExt4 core 持久化；VFS final eviction 与 recovery 触发 |

## 并行工作流与依赖

路线分成三条并行工作流：

1. **Core architecture**：mount services、journal、metadata buffer、allocator、extent 和
   per-inode state，由 KExt4 主线推进。
2. **Shared VFS/MM contracts**：PageCache invalidation、errseq 和 mount lifecycle，由对应
   负责人实现通用接口；KExt4 只提供需求和适配。
3. **Validation**：最小 sentinel、normal fio、fault/powercut 和长期 workload，按阶段逐级
   扩大，不把最终矩阵复制到每一个早期提交。

依赖关系：

```text
N0 baseline
    |
    v
N1 persistent journal / mount ownership --------+
    |                                            |
    v                                            |
N2 buffered runtime / concurrency <--- VFS/MM PageCache contracts
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

### N1：建立 mount ownership 与 persistent journal 框架

状态：下一阶段。

目标：消除“每个 mutation 新建 journal 并同步 checkpoint”的核心结构问题，为后台执行和
细粒度并发建立稳定 ownership。N1 首先同步驱动状态机，不要求立即创建 worker。

实现项：

- 将 immutable geometry/device capability 与 journal、metadata cache、allocator 等可变
  service state 从 catch-all mutation API 中分离；
- mount 生命周期内持有一个 persistent `JournalCoordinator`；
- coordinator 明确表达 running、committing、checkpointing 和 aborted 状态；
- transaction handle 按 credits 加入 running transaction，commit trigger 与 checkpoint
  progress 分离；
- metadata buffer 保留 running owner、committing frozen image 和 checkpoint image；
- 建立 per-inode sync/datasync transaction id 和 ordered-data dependency 的最小模型；
- 显式 `fsync/syncfs` 可以按 inode/filesystem 推进相关 transaction；
- 普通 mutation 返回前不再无条件完成 home-block checkpoint。

退出条件：

- 连续且不冲突的 metadata mutation 可加入同一个 running transaction；
- running transaction 冻结后可以开始新的 running transaction；
- commit 与 checkpoint 是两个可独立推进、可等待的阶段；
- 显式 filesystem sync 能同步驱动所有 pending state 到稳定存储；
- 最小 sentinel 覆盖未提交 transaction 不 replay、已提交 transaction 可 replay、ordered
  data 先于相关 metadata commit；
- 代码和文档明确哪个对象拥有 device、journal sequence、metadata buffer 和 checkpoint queue。

### N2：建立 buffered runtime 与并发框架

目标：把 PageCache、delalloc、ordered transaction、后台执行和局部锁连成主数据路径，达到
normal-path buffered fio 可用，而不是先追求所有故障边界。

实现项：

- per-inode logical-range state 表达 hole/delayed/unwritten/written、dirty 和 writeback；
- reservation 与 allocator accounting 使用 range ownership，不继续完善临时 per-block
  bridge ledger；
- writeback batch 关联 ordered-data dependency 和目标 transaction；
- 引入 background commit/checkpoint/writeback 与 dirty/journal-space pressure；
- 去除 bridge 的 mount-wide core mutex，建立 per-inode、journal、metadata-buffer 和
  per-group allocator 锁域；
- extent mutation 从 rebuild foundation 演进为 path-local find/insert/split/remove；
- allocator 建立稳定的 group/buddy/preallocation ownership；
- 保持 writeback、fdatasync、fsync 和 syncfs 不同 durability intent。

G2 退出条件：

- fixed normal-path buffered fio：seq write/read verify、rand write/read、2/4-job randrw 和
  fsync smoke 可重复通过；
- 多 inode I/O 不再被一个 mount-wide mutex 全部串行；
- 普通 mutation 不再强制 home-block checkpoint；
- 记录 commit/checkpoint/flush 次数、dirty/reservation 峰值和基础 fio 指标；
- 本阶段不以完整 fault、powercut、errseq 或 clean unmount matrix 作为退出条件。

### N3：可靠性、错误观察与 lifecycle 加固

目标：在最终异步执行图上补齐失败语义，而不是加固已经被替换的同步模型。

实现项：

- AddressSpace writeback error、SuperBlock filesystem error 和 per-file sample/cursor；
- journal/checkpoint abort 后的 forced-readonly 和新写入 gate；
- KVFS clean unmount/freeze：阻止新引用或写入，drain PageCache、inode metadata、running
  transaction、commit/checkpoint worker 和 orphan，再 detach/freeze；
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
- rsext4 只用作对比，不为其新增修复。

### N5：高级 I/O 与常用功能

目标：在稳定的 buffered/journal/lifecycle 架构上补齐常见 Linux I/O 和 metadata 语义。

- shared writable mmap、write fault、msync 和 truncate coherence；
- direct I/O、buffered/direct overlap 和 ordered-data completion；
- fallocate、preallocation、punch-hole 及其他按 workload 需要的 range operations；
- xattr syscall surface、ACL permission/inheritance、security namespace policy；
- HTree write scalability、large directory 和常见 ext4 feature；
- 每项格式能力必须同时考虑 feature negotiation、checksum、journal credits、recovery 和
  Linux/e2fsprogs 互操作。

### N6：替换门槛

目标：以持续回归和迁移方案替换 rsext4，而不是继续双线扩展。

退出条件：

- 常见 rootfs、package/build、fio、fsstress 和目标应用长期稳定；
- syscall、format、fault/recovery 和 performance matrix 进入持续 CI；
- 有默认后端切换、回退和镜像兼容说明；
- 达到 G4 后才停用旧 backend。

## 交给 VFS/MM 负责人的公共需求

这些需求可以与 N1 并行，不要求采用旧 S2 原型的具体实现：

1. **PageCache invalidation contract**：truncate、ordinary invalidation、reclaim 和 final
   teardown 可通知 filesystem address-space operation；明确 mapping/folio 锁上下文，callback
   不在 tree lock 下执行 backing I/O。
2. **Buffered prepare cancellation**：`write_begin()` 成功后所有退出路径与一次
   `write_end()` 配对；PageCache grow/copy 失败使用 `copied == 0`。
3. **Writeback errseq**：mapping error 向 filesystem-wide error 聚合；open file 分别采样
   file/superblock cursor；`fsync/syncfs` 按 Linux 语义检查并推进。
4. **Filesystem lifecycle**：unmount/freeze 能 gate 新引用/写入，drain VFS-owned data 和
   filesystem hook，只有成功后才 detach/标记 frozen，失败可以恢复 active state。

KExt4 bridge 只消费这些通用接口，不为 KExt4 建立私有 VFS/PageCache 旁路，也不要求修改
rsext4。

## 分层验证策略

### L0：每个代码切片

- KExt4 配置的 format/build/clippy；
- 触及状态机的窄 unit tests；
- diff review：ownership、sleepability、锁顺序、错误传播和文档同步。

### L1：框架 sentinel

- mount、一个普通 mutation、显式 sync、remount；
- committed/uncommitted journal replay；
- ordered-data 的最小可观察顺序；
- 不扩大为每个 syscall 的 fault matrix。

### L2：normal workload

- 固定 buffered fio smoke 和少量 metadata workload；
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

当前唯一主线是 N1，而不是继续旧 S2 或提前展开 N3：

1. 审计 `Ext4Filesystem` 中 geometry/device、journal、metadata buffer、allocator 和 mutable
   counters 的 ownership；
2. 引入 mount-owned persistent journal coordinator，第一步仍由调用者同步推进；
3. 把 transaction start/join、commit trigger 和 checkpoint progress 从
   `metadata_journal()`/mutation helper 中分离；
4. 在 coordinator 稳定后加入 per-inode sync tids 和 ordered-data dependency；
5. N1 完成后再接入 VFS/MM 负责人提供的 PageCache contract，进入 N2。

旧 `refactor-spec.md` 中仍有价值的 metadata buffer/JBD2、path-local extent、mballoc、
delalloc、background checkpoint、errseq、direct-I/O 和 fault matrix 内容已按上述依赖重新
归入 N1-N5；旧 R0-R11 和当前废弃的 S2-S8 顺序不再继续维护。
