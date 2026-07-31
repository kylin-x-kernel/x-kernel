# KExt4 — 安全与可靠性分析

## 信任模型

KExt4 只把块设备视为 I/O 传输层，不信任其中的元数据天然有效。来自磁盘的
superblock、group descriptor、directory block、extent tree、xattr block、bitmap 和
journal record 都属于外部输入，必须在解释或修改前完成范围、格式和 checksum 校验。

superblock feature bitmap 在进入文件系统逻辑时被包装为按 compat 类别区分的强类型
flags。未知位不会在解码时丢弃，incompat 未支持位仍会导致挂载失败，从而避免因类型
封装而放宽对不可信磁盘 feature 的校验。

KVFS bridge 信任 KVFS 已经提供内核拥有的 path name、dentry、inode `AddressSpace`
（含私有 `PageCache` storage）和文件生命周期回调。KExt4 核心不直接解引用用户态指针。

## 外部边界 / 攻击面

相关边界包括：

- ext4 块设备镜像，包括恶意或损坏的元数据；
- JBD2 journal replay record 和 checksum 字段；
- 从磁盘读取的 directory name 和 dirent file type；
- extent tree entry 和 block mapping；
- xattr name、value、external xattr block header、refcount 和 checksum；
- KVFS 运行态操作，例如 create、unlink、rename、truncate、writeback、fsync 和 syncfs；
- transaction commit、journal replay 或 checkpoint 期间的设备写入/flush 失败。

经检查，本 crate 使用 `#![forbid(unsafe_code)]`，不直接访问 MMIO/PIO，不管理 DMA
所有权，不使用 FFI、inline assembly 或架构专有裸接口。

## unsafe 代码清单

无。`fs/filesystems/kext4/src/lib.rs` 对整个 crate 禁止 unsafe code。Unsafe 或设备专有
操作位于本 crate 边界之外。

## 内存安全不变量

- 对 metadata bytes 切片前必须校验磁盘 offset 和 size。
- 信任 checksum-protected block 前必须完成 metadata checksum validation。
- block number 和 inode number 必须通过 layout、group、bitmap 和 system-zone 规则校验。
- JBD2 handle 在 dirty 或 revoke metadata buffer 前必须拥有足够 credits。
- mount 生命周期内的 `MountedJournal` 是生产路径唯一 journal identity，同时拥有磁盘 mapping、
  活跃 superblock 状态、transaction engine 和 FIFO checkpoint queue。同一个 runtime
  transaction 必须按 Running → Committing → Checkpoint → Finished 迁移；队列只保存该对象，
  持久化证据属于其 Checkpoint phase，不复制 payload 或携带可替换的 coordinator 引用。
- replay 完成后必须先把 transaction engine 的 next sequence、运行态 ring 和 checkpoint
  水位重置到 replay report，再开始 orphan cleanup；否则旧 sequence 不得用于创建新 transaction。
- internal-journal extent 必须连续且至少覆盖 JBD2 `s_maxlen` 声明的 logical block 数；
  inode-backed journal 的预分配容量可以大于 `s_maxlen`。每个 physical extent 都必须位于
  filesystem block count 内；磁盘映射校验完成前不得创建 `MountedJournal`。
- 多个 handle 可以独立退出并共享 running transaction；handle stop 必须归还未使用 credits。
  预期错误必须在该 handle 首次 metadata access 前完成校验并原样返回，不能改变其他 handle
  已发布的 bytes、dirty buffer ownership 或 transaction membership。单个 handle 不得在
  失败路径上删除已发布的 metadata/revoke membership，因为同一 block 可能已被其他 handle
  共享。任何普通错误若发生在 metadata 已发布之后，都按遗漏 preflight/unwind 的内部不变量
  失败处理并永久 abort journal；不得回滚同一 running transaction 中其他已经成功返回的
  operation，也不得把失败报告成可继续运行的局部回滚。尚未发布的私有状态由 ext4 算法显式
  unwind。
  普通 transaction 上界按 journal descriptor/commit/revoke 开销保守限制，不能用固定
  operation 数代替容量约束。显式 filesystem sync 必须先提交 running transaction，再推进
  committed checkpoint queue。
- inode sync/datasync tid 应属于 VFS runtime inode identity。当前接口尚未承载该状态，因此
  `fsync/fdatasync` 保守提交整个 running transaction 并 flush；不得以 inode number 为 key
  在 journal 中保存 mount-wide cursor，以免 inode number 复用继承旧 durability 状态。
- truncate 必须在释放旧 block mapping 前强制提交 orphan + `i_disksize` transaction；恢复期
  orphan cleanup 不得与普通 transaction 混合，并须在保持 recovery evidence 的策略下同步
  完成 commit/checkpoint。
- 多个 committed transaction 驻留磁盘日志时，sequence/start 必须保持指向最老 live
  transaction，追加只能从 mount 运行态 append head 开始并至少保留一个空 journal block；
  clean journal 首次提交必须先持久化非零 start 再发出 descriptor/data/commit，使激活与
  commit block 之间的掉电被恢复路径识别为未完成 active transaction；
  活跃期不得把运行态 head 持久化为 `s_head`，只有 clean journal 才写该字段；home-block flush 完成后
  才能把 tail 推进到下一个 transaction，队列非空时不得清除 ext4 recovery evidence。包含
  primary superblock 的 frozen checkpoint image 必须保留 recovery feature，不能让较老
  checkpoint 覆盖后续 live journal 的恢复标志。
- 缓存的 journal-superblock image 必须覆盖一个完整 journal block，且只能来自成功解码和
  checksum 校验的输入；每次内存更新必须重新解码生成下一份可信状态。journal write batch
  不得超过 128 KiB，不得跨越 ring wrap 或 internal-journal 不连续 physical extent。批量写入
  只改变请求粒度，不能合并 clean-journal activation flush 与 transaction 最终 durability
  flush，也不能省略没有 metadata commit 代为提供的同步 flush。
- extent 局部更新必须同时刷新被修改叶子、祖先索引、inode root 和 metadata checksum；
  ordered writeback 不得在同一 handle 内从路径更新切换到全树重写，credits 上界按本次写入
  规模与最大路径深度覆盖逐层分裂，且不得截断计算结果。
- range mutation 只有在完整操作可由单条 `ExtentPath` 表达时才能开始局部 metadata 写入；
  跨叶回退必须发生在首个 metadata write/create/revoke 之前。
- 不带 revoke feature 的 journal 只能在“旧 transaction 已全部 checkpoint”时无 revoke
  释放 metadata block；forget 同时必须从当前 handle 的 metadata 集合中移除该 block。Clean v2
  journal 必须先 flush revoke feature 才能允许 transaction/checkpoint 重叠；v1 journal 保持
  同步 checkpoint 退化路径。
- 内存中的 counter 和 descriptor 只应在同一事务内完成对应 metadata bytes staging 后更新。
- block allocation/release 必须在同一 metadata mutation 中同步 primary superblock 与 group
  descriptor 的 free-block counter；delayed-allocation admission 依赖 primary counter，并另行
  扣除 ext4 reserved blocks 和 bridge 尚未落盘的 reservation。
- 从磁盘解码的 external xattr name 必须拒绝内嵌 NUL，保持与 Linux/e2fsck 的 corruption
  handling 一致。
- 运行态 inode allocation 必须使用 bridge 已通过 `inode_init_owner()` 导出的显式
  UID/GID；KExt4 core 不得把新 inode owner 默认为 root。

## 线程安全

运行态 bridge 通过挂载级 mutex 串行化核心操作，因此当前 live mutation 不会并行进入同一个
`Ext4Filesystem` 核心。内部 JBD2 和 metadata-buffer 状态仍会记录 transaction ownership，
避免同一事务内出现冲突的 metadata access。同一 inode 的 writeback pass 另由 bridge mutex
串行化；writeback 扫描 PageCache 时不持有 core mutex，batch writer 只在 PageCache 释放
mapping/folio mutex 后进入核心。后续引入细粒度锁时，必须保持该跨层边界，以及 buffer
ownership、journal handle、allocator bitmap 和 inode metadata 之间的顺序约束。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 损坏镜像中的 xattr entry name 带内嵌 NUL，却被当成合法属性读入 | 中 | 恶意或损坏镜像在 `e_name` 中编码 NUL | `decode_xattr_entries()` 拒绝包含 `0` 的 name，并有畸形 entry 回归测试 |
| T-02 | external xattr block 在 unlink 或 rename overwrite 后泄漏，或 inode 继续引用已释放 block | 中 | zero-link victim 的 `i_file_acl != 0` | zero-link eviction 先释放或递减 EA block refcount，清 `i_file_acl`，更新 `i_blocks`，再释放 inode |
| T-03 | 运行态 unlink 过早释放仍被打开 fd 引用的 inode | 中 | KExt4 运行态后端上 unlink 一个仍打开的 inode | namespace transaction 只持久化 nlink/orphan；已有 VFS identity 可用 `referenced_inode()` 继续 I/O，最后引用销毁才进入 superblock final eviction |
| T-04 | journal credits 估算不足或按 data blocks 过度估算，导致 metadata update 在事务中途失败或被错误拒绝 | 中 | namespace remove、writeback、truncate、preallocation discard 或 final eviction 修改 orphan、xattr、extent 和 inode metadata | namespace reservation 只覆盖 dirent、nlink 和 orphan metadata，不包含后续 final eviction；ordered writeback 固定为 path-local 算法并按 logical blocks 与最大路径深度估算，禁止固定上限截断；legacy final eviction 和 extent truncate 按实际 tree blocks、需要 revoke 的旧 tree blocks 与 affected groups 计算；victim 带 external xattr 时 final eviction 预算包含 EA 清理 |
| T-05 | 恶意 extent 或 bitmap metadata 导致释放不属于该 inode 的 block | 高 | 损坏镜像把 extent/xattr block 指向 system zone 或非法 group | release 或 mutation 前执行 block ownership、system-zone、bitmap 和 checksum 校验 |
| T-06 | 设备写入/flush 失败让部分 metadata 可见 | 中 | commit、replay、checkpoint、显式 sync 或 xattr update 期间设备失败 | journal abort 保留 recovery state 或 pending checkpoint；后续 sync/mutation 返回 aborted，不跨 syscall 回滚内存修改；测试覆盖 explicit-sync commit、checkpoint、xattr 和 replay failure |
| T-07 | clean journal 上残留 legacy orphan，mount 永久返回 `NeedsRecovery` | 中 | namespace transaction 已 checkpoint，但 final inode eviction 尚未发生 | 显式 recovery 无论 journal 是否需要 replay 都遍历 legacy orphan；clean 分支以 `PreserveDuringRecovery` 建立 recovery evidence，逐个同步 commit/checkpoint，确认 journal start 清零后才清除 recovery feature；zero-link entry 复用 journaled final-eviction 路径 |
| T-08 | free-block aggregate 与 group descriptor 漂移导致 delayed-allocation 过量预留 | 中 | allocation/release 只更新一侧 counter，或发布后才发现普通错误 | block mutation 先在私有 bytes/cache 副本完成校验，再同时发布 superblock 与 group descriptor；发布后错误永久 abort journal；bridge 再扣除 ext4 reserved blocks 和运行态 reservation，`statfs()` 保留 group fold 作为独立统计路径 |
| T-09 | 新建 inode 固定为 root，绕过调用者 owner 语义 | 高 | bridge 丢弃 credential 或 core constructor 隐式填入 UID/GID 0 | create/mkdir/mknod/symlink callback 使用 `inode_init_owner()`，显式 `uid`、`gid` 随同 namei transaction 持久化 |
| T-10 | journal head 追上 tail 并覆盖尚未 checkpoint 的 commit | 高 | 多个 committed transaction 占满环形日志，追加仍继续写入 | 依据 oldest tail/current head 计算 live 空间，始终保留一个空 block；空间不足时先 checkpoint 最老 transaction 再重试，不发出覆盖写；真实 ext4 镜像测试覆盖双 transaction tail 推进 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 对 Linux ext4 有效 feature 返回 unsupported | EA inode、bigalloc、orphan-file、inline-data write 等 feature 尚未实现 | 当前操作失败 | filesystem 仍保持可审计状态，但该能力不可用 | 3 | feature negotiation 和显式 `UnsupportedKind` |
| F-02 | journal commit 或 checkpoint 失败 | 设备写入/flush 错误 | committing 或 pending checkpoint 状态保留并 abort journal | 当前 mount 后续 sync/mutation 拒绝继续，重新挂载时依赖 recovery | 2 | 失败返回前永久 abort；保留 recovery evidence 和 pending state，禁止后续 sync 假成功 |
| F-03 | recovery-time zero-link cleanup 失败 | crash 发生在 namespace commit 之后、final cleanup 之前，且 metadata 损坏、设备失败或 inode 使用尚未支持的格式 | recovery 保留 orphan/recovery evidence 并返回错误 | filesystem 不会被当作可写 root 暴露 | 2 | legacy orphan cleanup 使用独立 journal transaction；checkpoint 后在保留 recovery flag 的同时重载 mutable metadata state，成功后再继续下一个 orphan；clean-journal flush-failure 回归测试验证失败可观察、证据保留和重试成功，N3 在最终执行图上补齐其余 fault/powercut 矩阵 |
| F-04 | 粗粒度 mutex 串行化慢 I/O | live bridge 在 blocking filesystem work 周围持有 mount-level core lock | 吞吐下降 | 其他 KExt4 operation 等待 | 4 | N1 固定 `MountedJournal` 生命周期；N2 根据实际 worker 和共享状态建立 per-inode/per-group/journal/metadata-buffer 锁域；锁拆分前不宣称并发性能 |
| F-05 | journal reservation 空间不足 | operation 的实际 metadata targets 超过空 journal 容量，或 reservation 混入另一个 lifecycle 阶段的工作 | transaction 在修改 metadata 前失败 | 正常 writeback、fsync、rename-overwrite、truncate 或 orphan cleanup 返回错误；已有磁盘状态保持不变 | 3 | namespace 与 final eviction 分开预算；ordered writeback 只走 path-local update，并按最大路径深度计算不截断预算；跨叶 truncate 在首个 metadata 写入前选择按 tree blocks/groups 估算的全树路径；由 rename-overwrite、fragmented-writeback、balanced-split 和 preallocation-tail credit 回归测试约束 |
| F-06 | PageCache 与 filesystem core 锁序反转 | writeback 持有 core mutex 等待 mapping mutex，同时 cache miss 持有 mapping mutex 进入 backing read | 并发 buffered I/O 和 `sync()` 停止推进 | watchdog 报告 mutex deadlock，filesystem workload 无法继续 | 2 | bridge 不在 PageCache traversal 外层持有 core mutex；同 inode writeback 独立串行化，batch callback 在 mapping/folio mutex 释放后才进入核心；VFS/MM 后续仍需消除 tree lock 下的 backing I/O |
| F-07 | committed journal 占满可追加空间 | checkpoint 落后于 commit，head 接近 oldest tail | 新 transaction 暂时不能持久化 | mutation 等待 checkpoint progress | 3 | append 前按环形 live range 校验空间并保留一个空 block；提交路径捕获 `JournalBusy`，同步推进最老 pending checkpoint 后重试 |

## 故障管理

KExt4 使用 `Ext4Result` 和 typed `Ext4Error` 表达 corruption、unsupported format、
out-of-bounds metadata、checksum mismatch、journal credit exhaustion 和 device error。
日志化修改路径把普通失败前移到首次 metadata access 前；多个 handle 的空失败不会影响同一
running transaction 中其他修改。metadata 已发布后出现普通错误、设备/checksum/状态机错误或
handle accounting 错误时会 permanent abort journal，不执行跨 operation 的内存回滚。
Commit/checkpoint failure 会保留 committing 或 pending work 并永久 abort journal，避免静默
丢失尚未落盘的 metadata；后续 sync 和 mutation 都观察 `JournalAborted`，不能把内存中的
transaction 当作 durable。多个 pending commit 存在时，完成最老 transaction 只推进 journal
tail；只有最后一个完成后才清除 journal start 和 ext4 recovery evidence。当前 inode sync
采用提交整个 running transaction 的保守语义。显式 recovery 同样只有在 legacy orphan
cleanup 全部完成 commit/checkpoint、确认 journal start 为零，并 flush 最终 superblock
状态之后才能返回成功；在 cleanup durable 之前失败会保留 orphan head 或 recovery feature
作为下一次恢复入口，最终状态 flush 失败也必须向调用方返回错误。

N1 已完成的 `MountedJournal` 边界必须继续保留 recovery evidence、revoke 和
checkpoint-failure-retain 不变量。后续 N2 后台化不能以暂时缺少 N3 完整 fault matrix 为理由
吞掉错误或清除 pending state。用户态 sticky error/errseq 和 forced-readonly 的完整联动在
后台执行图稳定后的 N3 收口。

## 隐私分析

KExt4 会存储并返回 filesystem data 和 metadata，其中 xattr value 可能包含 security label
或应用数据。核心不记录 xattr value 或文件内容日志。

## 已知限制

- 运行态 xattr syscall hook 尚未接入，因为 KVFS 还没有 xattr ops trait。
- POSIX ACL 当前只是 opaque xattr bytes，不实现 ACL permission enforcement 或 inheritance。
- EA inode、oversized xattr、bigalloc、orphan-file、inline-data write、huge-file write
  block-unit accounting、encryption/casefold、direct I/O 和 mmap coherence 仍是后续工作。
- KVFS unmount 已在 topology detach 前后同步 VFS 与 journal 状态，但阻止新引用/写入、
  lazy unmount、freeze 和后台 worker drain 仍需最终 lifecycle gate。
- 精准 per-inode `fsync/fdatasync` 等待需要 KVFS runtime inode 承载 sync/datasync tid；当前
  保守提交整个 running transaction，正确但可能额外提交无关 inode 的 metadata。
- 新增或扩展 mutation 时必须继续证明所有预期失败可在首次 metadata access 前发现；若无法
  做到，需要实现 path-local 显式 unwind，不能恢复 operation savepoint。遗漏路径会保守地
  abort 整个 running transaction，保证正确性但降低该次 mount 的可用性。
- 当前通用 block 接口仍是逐请求同步完成路径；多请求 in-flight、完成通知和 VirtIO 中断驱动
  等待属于 block/driver 外部依赖，KExt4 只能先减少并聚合请求。

## 审计清单

- metadata parser 是否拒绝 truncated、out-of-bounds、unsorted 或 checksum-invalid input？
- 每个 mutation 是否为所有可能 dirty 或 revoke 的 metadata block 预留了足够 journal credits？
- zero-link cleanup 是否在一个可审计事务中释放 data、external xattr block、orphan entry 和
  inode bitmap state？
- zero-link inode 是否只能由既有 VFS identity 访问，并在最后引用消失前保持 inode number、
  extent 和 xattr 有效？
- 文档是否区分了 core support、live KVFS syscall exposure 和路线图能力？
