# KExt4 — 安全与可靠性分析

## 信任模型

KExt4 只把块设备视为 I/O 传输层，不信任其中的元数据天然有效。来自磁盘的
superblock、group descriptor、directory block、extent tree、xattr block、bitmap 和
journal record 都属于外部输入，必须在解释或修改前完成范围、格式和 checksum 校验。

superblock feature bitmap 在进入文件系统逻辑时被包装为按 compat 类别区分的强类型
flags。未知位不会在解码时丢弃，incompat 未支持位仍会导致挂载失败，从而避免因类型
封装而放宽对不可信磁盘 feature 的校验。

KVFS bridge 信任 KVFS 已经提供内核拥有的 path name、dentry、PageCache object 和文件
生命周期回调。KExt4 核心不直接解引用用户态指针。

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
- 不带 revoke feature 的 journal 只能在“旧 transaction 已全部 checkpoint”时无 revoke
  释放 metadata block；forget 同时必须从当前 handle 的 metadata 集合中移除该 block。
- 内存中的 counter 和 descriptor 只应在同一事务内完成对应 metadata bytes staging 后更新。
- block allocation/release 必须在同一 metadata mutation 中同步 primary superblock 与 group
  descriptor 的 free-block counter；delayed-allocation admission 依赖 primary counter，并另行
  扣除 ext4 reserved blocks 和 bridge 尚未落盘的 reservation。
- 从磁盘解码的 external xattr name 必须拒绝内嵌 NUL，保持与 Linux/e2fsck 的 corruption
  handling 一致。

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
| T-04 | journal credits 估算不足或按 data blocks 过度估算，导致 metadata update 在事务中途失败或被错误拒绝 | 中 | namespace remove、writeback、truncate、preallocation discard 或 final eviction 修改 orphan、xattr、extent 和 inode metadata | namespace 与 final eviction 使用独立 transaction/credit budget；ordered writeback 和 extent truncate 按当前/预计 tree blocks、需要 revoke 的旧 tree blocks 与 affected groups 计算 metadata targets，而不是仅按 data block 数或 `i_blocks`；victim 带 external xattr 时 final eviction 预算包含 EA 清理 |
| T-05 | 恶意 extent 或 bitmap metadata 导致释放不属于该 inode 的 block | 高 | 损坏镜像把 extent/xattr block 指向 system zone 或非法 group | release 或 mutation 前执行 block ownership、system-zone、bitmap 和 checksum 校验 |
| T-06 | 设备写入/flush 失败让部分 metadata 可见 | 中 | commit、replay、checkpoint 或 xattr update 期间设备失败 | journal abort/rollback 保留 recovery state 或 pending checkpoint；测试覆盖 xattr fault retry 和 replay failure |
| T-07 | clean journal 上残留 legacy orphan，mount 永久返回 `NeedsRecovery` | 中 | namespace transaction 已 checkpoint，但 final inode eviction 尚未发生 | 显式 recovery 无论 journal 是否需要 replay 都遍历 legacy orphan；zero-link entry 复用 journaled final-eviction 路径 |
| T-08 | free-block aggregate 与 group descriptor 漂移导致 delayed-allocation 过量预留 | 中 | allocation/release 只更新一侧 counter，或 rollback 未恢复同一 savepoint | block mutation 同时更新 superblock 与 group descriptor；bridge 再扣除 ext4 reserved blocks 和运行态 reservation，`statfs()` 保留 group fold 作为独立统计路径 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 对 Linux ext4 有效 feature 返回 unsupported | EA inode、bigalloc、orphan-file、inline-data write 等 feature 尚未实现 | 当前操作失败 | filesystem 仍保持可审计状态，但该能力不可用 | 3 | feature negotiation 和显式 `UnsupportedKind` |
| F-02 | journal commit 后 checkpoint 失败 | 设备写入/flush 错误 | pending checkpoint 留在队列 | 后续 sync/unmount 需要重试，必要时依赖 recovery | 2 | checkpoint failure retain semantics 和 sync retry |
| F-03 | recovery-time zero-link cleanup 失败 | crash 发生在 namespace commit 之后、final cleanup 之前，且 metadata 损坏、设备失败或 inode 使用尚未支持的格式 | recovery 保留 orphan/recovery evidence 并返回错误 | filesystem 不会被当作可写 root 暴露 | 2 | legacy orphan cleanup 使用独立 journal transaction；checkpoint 后在保留 recovery flag 的同时重载 mutable metadata state，成功后再继续下一个 orphan，N3 在最终执行图上补齐 fault/powercut 矩阵 |
| F-04 | 粗粒度 mutex 串行化慢 I/O | live bridge 在 blocking filesystem work 周围持有 mount-level core lock | 吞吐下降 | 其他 KExt4 operation 等待 | 4 | N1 先拆分 mount service ownership，N2 再建立 per-inode/per-group/journal/metadata-buffer 锁域；锁拆分前不宣称并发性能 |
| F-05 | journal reservation 空间不足 | operation 的实际 metadata targets 超过空 journal 容量，或 credit planner 忽略 external extent tree 的重写成本 | transaction 在修改 metadata 前失败 | 正常 writeback、fsync、truncate 或 orphan cleanup 返回错误；已有磁盘状态保持不变 | 3 | ordered writeback 按当前与预计 tree shape 估算；extent truncate 按 tree blocks 和不同 groups 估算；由 fragmented-writeback 和 preallocation-tail credit 回归测试约束 |
| F-06 | PageCache 与 filesystem core 锁序反转 | writeback 持有 core mutex 等待 mapping mutex，同时 cache miss 持有 mapping mutex 进入 backing read | 并发 buffered I/O 和 `sync()` 停止推进 | watchdog 报告 mutex deadlock，filesystem workload 无法继续 | 2 | bridge 不在 PageCache traversal 外层持有 core mutex；同 inode writeback 独立串行化，batch callback 在 mapping/folio mutex 释放后才进入核心；VFS/MM 后续仍需消除 tree lock 下的 backing I/O |

## 故障管理

KExt4 使用 `Ext4Result` 和 typed `Ext4Error` 表达 corruption、unsupported format、
out-of-bounds metadata、checksum mismatch、journal credit exhaustion 和 device error。
日志化修改路径在支持的范围内 abort transaction，并 rollback 已暂存的 undo state。
Checkpoint failure 会保留 pending work，避免静默丢失尚未落盘的 metadata。

N1 引入 persistent journal coordinator 时必须保留现有 recovery evidence、undo/revoke 和
checkpoint-failure-retain 不变量。N1 只改变 ownership 和状态推进方式，不以暂时缺少 N3
完整 fault matrix 为理由吞掉错误或清除 pending state。用户态 sticky error/errseq 和
forced-readonly 的完整联动在后台执行图稳定后的 N3 收口。

## 隐私分析

KExt4 会存储并返回 filesystem data 和 metadata，其中 xattr value 可能包含 security label
或应用数据。核心不记录 xattr value 或文件内容日志。

## 已知限制

- 运行态 xattr syscall hook 尚未接入，因为 KVFS 还没有 xattr ops trait。
- POSIX ACL 当前只是 opaque xattr bytes，不实现 ACL permission enforcement 或 inheritance。
- EA inode、oversized xattr、bigalloc、orphan-file、inline-data write、huge-file write
  block-unit accounting、encryption/casefold、direct I/O、mmap coherence 和完整 live unmount/freeze
  仍是后续工作。

## 审计清单

- metadata parser 是否拒绝 truncated、out-of-bounds、unsorted 或 checksum-invalid input？
- 每个 mutation 是否为所有可能 dirty 或 revoke 的 metadata block 预留了足够 journal credits？
- zero-link cleanup 是否在一个可审计事务中释放 data、external xattr block、orphan entry 和
  inode bitmap state？
- zero-link inode 是否只能由既有 VFS identity 访问，并在最后引用消失前保持 inode number、
  extent 和 xattr 有效？
- 文档是否区分了 core support、live KVFS syscall exposure 和路线图能力？
