# KExt4 — 安全与可靠性分析

## 信任模型

KExt4 只把块设备视为 I/O 传输层，不信任其中的元数据天然有效。来自磁盘的
superblock、group descriptor、directory block、extent tree、xattr block、bitmap 和
journal record 都属于外部输入，必须在解释或修改前完成范围、格式和 checksum 校验。

superblock feature bitmap 在进入文件系统逻辑时被包装为按 compat 类别区分的强类型
flags。未知位不会在解码时丢弃，incompat 未支持位仍会导致挂载失败，从而避免因类型
封装而放宽对不可信磁盘 feature 的校验。

KExt4 当前只对 extent-backed block map 提供完整 mutation 合同，因此缺少
`EXT4_FEATURE_INCOMPAT_EXTENTS` 的镜像会在挂载状态发布前失败。这个检查防止合法但超出
实现能力的 legacy indirect 格式先进入可写运行态，再由延迟 writeback 把用户态已经成功的
写入转化为异步 `EOPNOTSUPP`、journal shutdown 错误或部分持久化。即使 superblock 启用了
extents feature，个别 legacy inode 的 `write_begin`/`page_mkwrite` 也会在 delayed-allocation
reservation 和用户数据标脏前同步拒绝。

KExt4 的 KVFS operation 实现信任 KVFS 已经提供内核拥有的 path name、dentry、inode `AddressSpace`
（含私有 `PageCache` storage）和文件生命周期回调。KExt4 核心不直接解引用用户态指针。
它也信任 `fill_super` 只接收 KVFS 已按 canonical `(s_type, BlockDevice)` reservation 并持有
独占 claim 的 nascent superblock；KExt4 不建立平行的 device-to-mount cache。

## 外部边界 / 攻击面

相关边界包括：

- ext4 块设备镜像，包括恶意或损坏的元数据；
- JBD2 journal replay record 和 checksum 字段；
- 从磁盘读取的 directory name 和 dirent file type；
- extent tree entry 和 block mapping；
- KVFS FIEMAP 请求范围，以及 core 从磁盘 mapping 与 inode extent-status 区间统一报告的 extent；
- xattr name、value、external xattr block header、refcount 和 checksum；
- KVFS 运行态操作，例如 create、unlink、rename、truncate、writeback、fsync 和 syncfs；
- KVFS 传入的 atime、mtime 和 ctime，以及磁盘 inode size/extra timestamp 字段决定的可表示范围；
- transaction commit、journal replay 或 checkpoint 期间的设备写入/flush 失败。

经检查，本 crate 使用 `#![forbid(unsafe_code)]`，不直接访问 MMIO/PIO，不管理 DMA
所有权，不使用 FFI、inline assembly 或架构专有裸接口。

## unsafe 代码清单

无。`fs/filesystems/kext4/src/lib.rs` 对整个 crate 禁止 unsafe code。Unsafe 或设备专有
操作位于本 crate 边界之外。

## 内存安全不变量

- 对 metadata bytes 切片前必须校验磁盘 offset 和 size。
- 信任 checksum-protected block 前必须完成 metadata checksum validation。
- Primary superblock 的 metadata checksum failure 必须先于 mount feature negotiation 返回；
  capability 拒绝只能发生在 checksum 通过后，并在首次 open 及 recovery reload 发布运行态前执行。
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
- ordered writeback 不得把全范围静态 mapping/space plan 当作执行真相。dirty range 的必需块
  已纳入 delayed-allocation reservation accounting；执行 cursor 在每个实际 mapping/allocation
  run 前重新查询 extent，并由 journal handle 自己约束 mounted credit limit。handle 扩展失败
  只能在完整 run 边界 flush 并提交 durable prefix。只有该 transaction 成功后才能释放对应
  delalloc range；因 metadata/fragmentation 发生的后续 ENOSPC 也必须按部分进度返回。未完成
  后缀及其 reservation 必须保留，optional preallocation budget 不能因 transaction restart 被
  重新授予，也不能占用 ext4 reserved blocks 或其他 inode 的 delalloc reservation。
  整体 writeback 在后续 transaction 失败时必须返回已完成字节前缀；PageCache 只结束其中完整
  folio 的 writeback，边界 folio 和后缀保持 dirty。普通可重试错误可以再次提交该 suffix；
  device/JBD2 错误会 abort 当前 mount journal，必须先 recovery/remount。
- range mutation 只有在完整操作可由单条 `ExtentPath` 表达时才能开始局部 metadata 写入；
  跨叶回退必须发生在首个 metadata write/create/revoke 之前。
- 不带 revoke feature 的 journal 只能在“旧 transaction 已全部 checkpoint”时无 revoke
  释放 metadata block；forget 同时必须从当前 handle 的 metadata 集合中移除该 block。Clean v2
  journal 必须先 flush revoke feature 才能允许 transaction/checkpoint 重叠；v1 journal 保持
  同步 checkpoint 退化路径。
- 内存中的 counter 和 descriptor 只应在同一事务内完成对应 metadata bytes staging 后更新。
- block allocation/release 必须在同一 metadata mutation 中同步 primary superblock 与 group
  descriptor 的 free-block counter；delayed-allocation admission 依赖 primary counter，并另行
  扣除 ext4 reserved blocks 和 core 持有的 delayed-allocation mount aggregate。
- `Ext4StatFsMode` 只能改变对外总块数的口径；free、available、reserved-block 和
  delayed-allocation aggregate 必须保持同一唯一事实源，不能按 `minixdf` 重算或放宽空间准入。
- 从磁盘解码的 external xattr name 必须拒绝内嵌 NUL，保持与 Linux/e2fsck 的 corruption
  handling 一致。
- 目录记录长度必须由 `RawDirectoryEntry` 的唯一编解码入口按 Linux 磁盘语义解释。HTree root
  的固定字段、磁盘 hash version 和树深必须经共享 root decoder 校验；磁盘版本 `3/4/5` 是
  非法元数据，版本 `6` 是合法 SIPHASH 格式，但缺少 fscrypt name key 的 mutation 必须在依据
  该 hash 选择或写入 HTree leaf 前返回 `Unsupported`。
- HTree signedness 必须由 mount state 统一拥有：默认 hash 在 mount 时校验，`DIR_INDEX` 未启用
  时不得解释 signedness，显式 unsigned 优先于 signed，两者都未指定时遵循当前 Linux 的
  unsigned-char 选择；策略解析不得写盘，可写 mount 只能在 root inode 初始化成功后、superblock
  发布前持久化该默认，read-only mount 不得写盘。Orlov placement 必须固定使用 signed HALF_MD4
  和 superblock seed，不能复用 HTree default/signedness policy。
- 新写 external xattr block 必须按 Linux ext4 算法生成每项 `e_hash` 和聚合 `h_hash`，并在其后
  计算 metadata checksum；不能用只保证 KExt4 自身可读、但会被 e2fsck 判坏的零 hash 编码。
- Xattr name-only 遍历仍必须验证 entry 末端、value range、external block header 和 checksum；
  sink 只能在单次回调期间借用 name bytes，不能保存该引用。
- External xattr block 的 `h_reserved` 按 Linux 语义作为 opaque 字段处理，不能仅因其非零拒绝
  e2fsck 修复后的合法块；启用 metadata checksum 时仍由完整 block checksum 检测意外修改。
- 相同 xattr value 的允许替换必须在 journal credit 准备和 metadata access 前短路，不能更新
  inode ctime，也不能产生无意义的 journal 写入。
- 普通 xattr set/remove 必须固定使用当前 `i_extra_isize`，只改变目标 entry 的存储区域；可选的
  extra-isize 扩展只由统一 ordinary inode-dirty 入口触发，xattr apply 和 zero-link cleanup 必须
  绕过该入口以防递归。
- 扩大 `i_extra_isize` 前必须先证明全部 xattr 可在缩小后的 inode body 与一个 external block
  中完整分区；迁移、`i_file_acl`/`i_blocks` 和 inode checksum 更新必须属于同一 transaction。
  运行时 effective `want_extra_isize` 必须按 Linux 规则包含默认 32 字节，并仅在
  `RO_COMPAT_EXTRA_ISIZE` 存在时合并磁盘 min/want；新 inode 必须在分配事务中直接写入该值。
  effective want 无法满足时只能退到磁盘 `min_extra_isize` 或保留当前值，不能丢弃属性或阻断
  本来合法的普通 inode metadata 更新。迁移选择必须是有界的 entry-by-entry 扫描，并禁止把
  `system.data` 移到 external block。内容未变的已有 external block 必须直接复用；需要新
  external block 的候选必须在规划阶段检查 allocator 可用性，并在 apply 前完成分配。规划与分配
  之间的空闲空间竞争返回 `NoSpace` 时记录 resident `no_expand` 并继续普通更新，不能扩大为假
  `ENOSPC`；删除 xattr 清除该状态，journal 的暂时性 credit/忙碌失败不应永久禁止重试。设备 I/O、
  checksum 和 corruption 错误仍必须传播并触发既有 journal 失败策略，不能以 best-effort 为由吞掉。
- 运行态 inode allocation 必须使用 kext4 callback 已通过 `inode_init_owner()` 导出的显式
  UID/GID；KExt4 core 不得把新 inode owner 默认为 root。
- Filesystem 级 timestamp range 必须按 inode size 判断完整 `i_[acm]time_extra` 容量；128-byte
  inode 只能声明秒级精度和有符号 32-bit 秒范围。Core encode/decode 仍须按 inode 实际 extra
  field 存在性截断纳秒、限制 epoch，并把解码后的值作为唯一 resident metadata 结果发布。
- FIEMAP 的 logical/physical/length 乘法和加法必须 checked；遍历必须同时受请求末端与
  inode 创建时缓存的格式上限约束，损坏 mapping 不得越界输出。
- Resident identity 只属于 KVFS `VfsInode` 和 VFS-wide `(SuperBlock, ino)` table；KExt4
  不得建立 inode-number cache、
  `Live/Evicting/Evicted` 状态或基于 core handle 引用数的 last-reference 规则。`RawInode` 只能
  作为磁盘解码值，不能承载 runtime identity 或 ext4-private transient state。
- `VfsInode` 必须直接组合一份 `Ext4Inode` private component，不能只保存 inode number 并按操作重新
  解码。unlink/rmdir/rename 必须把已锁定 VFS victim/moved/replaced 的 private component 传入 ext4 算法，
  防止 open handle 与 namespace mutation 各自更新不同对象。
- Legacy orphan chain 的 `i_dtime` 必须从 journaled inode-table bytes 读取并原位更新；不得为
  resident 前驱按编号构造第二份 private state，也不得从可能过期的 private snapshot 恢复 next。
- delayed-allocation 的区间、per-inode reserved count 和 mount aggregate 全归 KExt4；一次
  reserve/release/truncate/writeback/eviction API 必须在 core mutation guard 内联合更新它们，
  kext4 KVFS 层不得维护 set、逐块节点或另一个 aggregate。
- KVFS 在 final hook 前发布 `Freeing`，因此普通 VFS 能力不能与 eviction Phase A/B/C 并存。
  Core 三个 phase 都借用 VFS inode 组合持有的同一 private component，不返回可逃逸的 eviction
  handle；inode slot 释放后由 KVFS 删除旧 cache entry，number reuse 构造全新的 private state。

## 线程安全

运行态 KVFS `SuperBlock` 把无状态 `Ext4SuperOperations` 与唯一 `RwLock<Ext4SbInfo>`
分别作为 `s_op`/`s_fs_info` 保存；`s_op` 是所有挂载共享的真正静态表，private object 由
superblock 独占且只能借用恢复，不存在承载 mount identity 的 operation wrapper 或可逃逸的
typed owner。所有 inode mapping 也共享静态 `Ext4AddressSpaceOperations`，callback 只从
`mapping->host` 借用唯一 `Ext4Inode`。仍需联合修改
superblock/group metadata 的 mutation 由挂载级 write guard 串行化；只读 core 操作共享进入，
delalloc range 与 mount aggregate 则在 shared guard 下由独立 reservation mutex 联合更新。KVFS
cache 合并并发 `iget` 并封闭 `New/Freeing`；ext4 private metadata 与 transient state 在 inode
state mutex 下更新，KExt4 不再取得另一把 inode-cache lock。内部 JBD2 和 metadata-buffer 状态仍会记录
transaction ownership，避免同一事务内出现冲突的 metadata access。同一 inode 的 writeback pass 另由 Ext4Inode mutex
串行化；writeback 扫描 PageCache 时不持有 core mutex，batch writer 只在 PageCache 释放
mapping/folio mutex 后进入核心。后续引入细粒度锁时，必须保持该跨层边界，以及 buffer
ownership、journal handle、allocator bitmap 和 inode metadata 之间的顺序约束。
FIEMAP 由 VFS inode shared lock 稳定单 inode mapping；core read lock 和 delayed-block
mutex 只分别短暂取得，并在安全 writer 访问用户页前释放。不得把用户内存访问放进挂载级
core lock 或 delayed-block mutex。Buffered write 和 shared write fault 都必须在 inode
exclusive data lock 下先把 hole reservation 发布到同一个 delayed set，不能以 dirty-folio
扫描替代 filesystem mapping prepare。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 损坏镜像中的 xattr entry name 或 value range 被 name-only list 路径当成合法属性 | 中 | 恶意或损坏镜像编码 NUL、越界/重叠 value 或无效 external block | `decode_xattr_entries()` 拒绝 NUL 和畸形 entry；name-only 遍历仍校验 value range、block header 与 checksum，并有回归测试 |
| T-02 | external xattr block 在 unlink 或 rename overwrite 后泄漏，或 inode 继续引用已释放 block | 中 | zero-link victim 的 `i_file_acl != 0` | zero-link eviction 先释放或递减 EA block refcount，清 `i_file_acl`，更新 `i_blocks`，再释放 inode |
| T-03 | 运行态 unlink 过早释放仍被打开 fd 引用的 inode | 中 | KExt4 运行态后端上 unlink 一个仍打开的 inode | namespace transaction 只持久化 nlink/orphan；open-file 持有唯一 VFS inode 及其 ext4 private state，最后 VFS 引用销毁才进入 superblock final eviction |
| T-04 | journal credits 估算不足或按 data blocks 过度估算，导致 metadata update 在事务中途失败或被错误拒绝 | 中 | namespace remove、HTree 转换/split、writeback、truncate、preallocation discard 或 final eviction 修改 orphan、xattr、extent 和 inode metadata | namespace 插入预检显式记录 HTree 路径，转换后立即 split 按两个独立单块 extent 预估并在 inode flag 发布前加入 HTree credits；namespace removal 只覆盖 dirent、nlink 和 orphan metadata，不包含后续 final eviction；ordered writeback 固定为 path-local 算法，由 live cursor 按实际 unwritten conversion 和 hole allocation run 续订 handle，达到 transaction 上限时提交完整前缀并重启；legacy final eviction 和 extent truncate 按实际 tree blocks、需要 revoke 的旧 tree blocks 与 affected groups 计算，目录/block-mapped symlink 数据块释放按释放块数逐一计入 revoke credits；victim 带 external xattr 时 final eviction 预算包含 EA 清理 |
| T-05 | 恶意 extent 或 bitmap metadata 导致释放不属于该 inode 的 block | 高 | 损坏镜像把 extent/xattr block 指向 system zone 或非法 group | release 或 mutation 前执行 block ownership、system-zone、bitmap 和 checksum 校验 |
| T-06 | 设备写入/flush 失败让部分 metadata 可见 | 中 | commit、replay、checkpoint、显式 sync 或 xattr update 期间设备失败 | journal abort 保留 recovery state 或 pending checkpoint；后续 sync/mutation 返回 aborted，不跨 syscall 回滚内存修改；测试覆盖 explicit-sync commit、checkpoint、xattr 和 replay failure |
| T-07 | clean journal 上残留 legacy orphan，mount 永久返回 `NeedsRecovery` | 中 | namespace transaction 已 checkpoint，但 final inode eviction 尚未发生 | 显式 recovery 无论 journal 是否需要 replay 都遍历 legacy orphan；clean 分支以 `PreserveDuringRecovery` 建立 recovery evidence，逐个同步 commit/checkpoint，确认 journal start 清零后才清除 recovery feature；zero-link entry 复用 journaled final-eviction 路径 |
| T-08 | free-block aggregate、delalloc 区间与 reservation aggregate 漂移导致过量预留 | 中 | VFS/ext4 两侧分别更新，或逐块 set 与 mount counter 只更新一侧 | block mutation 同时发布 superblock 与 group descriptor；ext4 range API 在独立 reservation mutex 下联合更新 inode interval、`i_reserved_data_blocks` 等价计数和 mount aggregate，admission/statfs 直接使用该 aggregate |
| T-09 | 新建 inode 固定为 root，绕过调用者 owner 语义 | 高 | callback 丢弃 credential 或 ext4 constructor 隐式填入 UID/GID 0 | create/mkdir/mknod/symlink callback 使用 `inode_init_owner()`，显式 `uid`、`gid` 随同 namei transaction 持久化 |
| T-10 | journal head 追上 tail 并覆盖尚未 checkpoint 的 commit | 高 | 多个 committed transaction 占满环形日志，追加仍继续写入 | 依据 oldest tail/current head 计算 live 空间，始终保留一个空 block；空间不足时先 checkpoint 最老 transaction 再重试，不发出覆盖写；真实 ext4 镜像测试覆盖双 transaction tail 推进 |
| T-11 | 损坏 extent 或超大 FIEMAP 范围造成算术溢出、错误物理地址或无界遍历 | 高 | disk mapping 长度越过格式容量，或字节/块换算未检查 | ext4 算法暴露 Linux 对等的 inode 格式相关最大字节数；KVFS operation 对每次加法、乘法和输出字段做 checked 校验，并把 mapping 截断到请求与格式边界 |
| T-12 | 磁盘 inode 的 immutable/append-only 状态在 VFS 接入中丢失，导致 xattr 被修改 | 中 | iget 构造 KVFS inode 时总是使用空 `NodeFlags` | ext4 以语义方法暴露 `EXT4_IMMUTABLE_FL/EXT4_APPEND_FL`；kext4 KVFS 层在发布 VFS inode identity 时映射为 KVFS flags，由通用 xattr 权限层在 mutation 前返回 `EPERM` |
| T-13 | 同一 inode 出现 snapshot 分叉，或 inode number reuse 继承旧 transient state | 中 | KVFS 初始化/释放期间并发 `iget`，namei 按编号重载 live child，或 orphan removal 为 resident 前驱解码临时对象 | KVFS-wide `(SuperBlock, ino)` table 的 `New/Live/Freeing` 是唯一 identity 状态；`New/Freeing` 等待并重试；kext4 向 unlink/rmdir/rename 传入既有 private component；legacy orphan next 只读写 journaled inode-table bytes；KExt4 无 resident cache；reuse 只能在 `Freeing` 完成并删除旧 slot 后发生 |
| T-14 | shrink 后重新增长暴露旧 PageCache 数据 | 高 | ext4 backing prepare 提前把 VFS `i_size` 改成目标值，导致 `truncate_setsize()` 误判长度未变并跳过 folio 丢弃或 EOF 清零 | regular-file metadata publish 只更新 `i_disksize`；唯一 `i_size` 由 VFS 在 PageCache 顺序点发布；core prepare 与 KVFS shrink/regrow、partial-grow 回归测试共同约束该职责边界 |
| T-15 | 扩大 `i_extra_isize` 时覆盖、丢失 xattr，生成 e2fsck 不接受的 external EA block，或在磁盘满时拒绝本可完成的更新 | 高 | xattr syscall 错误地同时改变 extra-isize、普通 inode dirty 路径漏掉扩展，新 extra fields 与旧 inline 区间重叠，external entry/block hash 留零，错误地 COW 内容未变的共享 block，或布局阶段的空闲块在 apply 前被占用 | 普通 set/remove 固定使用当前 extra-isize，统一 ordinary inode-dirty 入口再按 want/min/current 扩展；迁移采用 Linux 风格 entry-by-entry 选择且固定 `system.data`；内容未变的 external block 直接复用，需要新 block 的布局在 apply 前预分配并把 `NoSpace` 降级为 `no_expand`；同一 journal transaction 重写 external block、生成 Linux-compatible `e_hash`/`h_hash`、清理旧 inline 区并更新 inode checksum；单元测试覆盖 deferred expansion、目录 inode dirty、磁盘满共享 external 复用、迁移选择与独立固定 hash 向量 |
| T-16 | 损坏 legacy orphan head 让挂载恢复失败、循环，或访问未分配/reserved inode | 中 | `s_last_orphan` 越界或指向 allocation bit 为零的 inode，已加载 inode 带不可截断的链接类型，或 `i_dtime` 指向非法 next | 普通 orphan API 继续严格拒绝非法编号；显式 recovery 在 inode-table decode 前验证编号和带 checksum 的 bitmap，加载后验证可截断类型与 next，并以 `PreserveDuringRecovery` journal transaction 持久化清零后终止 bad chain；bitmap I/O/checksum、inode decode 和合法但未支持的 cleanup 继续失败而不被降级 |
| T-17 | `minixdf`/`bsddf` 改变 free-space 事实源或错误扣除 metadata overhead | 中 | 两种模式分别实现整套 statfs 公式，或 VFS integration 直接修改 free counters | typed mode 只选择 `blocks_count - overhead` 或原始 `blocks_count`；free/available/inode 统计走共享代码，Linux-image 回归测试验证两者除总块数外一致 |
| T-18 | 128-byte inode 对外暴露不存在的纳秒精度，或新建 inode 丢失纳秒/扩展 epoch 并在 2038 年后回绕 | 中 | 只在 inode-table encode 时截断、VFS 比较原始高精度时间，或初始化结构只保存 base `u32` 秒 | KExt4 按 inode size 返回共享的 `TimestampLimits`，由唯一 KVFS `SuperBlock` 持有；`VfsInode::current_time()` 在比较、callback 和 publication 前只截断一次；新建 inode 保存完整 `Ext4Timestamp` 并通过 base/extra encoder 写盘，128-byte 格式在边界钳制，256-byte 格式保留纳秒和 epoch；回归测试覆盖两种 inode size 的能力选择、初始化编码和 resident publication |
| T-19 | 合法 SIPHASH HTree 被误报损坏 | 中 | root `hash_version=6` 进入目录写路径 | 共享 HTree root decoder 只接受磁盘版本 `0/1/2/6`，HTree signedness 来自 mount state；SIPHASH 在需要密钥的 hash 边界返回 `Unsupported`，相关事务不能提交 metadata mutation |
| T-20 | 未标 signedness 的旧 HTree 镜像跨 Linux 挂载后名称不可见，或 Orlov 目录放置漂移 | 高 | 磁盘两种 signedness flag 都为空时按 signed 计算，或 Orlov 复用 HTree default/unsigned policy | mount state 按 Linux unsigned-char 语义生成 `hash_unsigned`，RW 持久化 unsigned flag、RO 保持磁盘不变；HTree 只经 mount policy hash，Orlov 独立固定 signed HALF_MD4 + seed；高位字节名称回归测试区分 signed/unsigned |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 对 Linux ext4 有效 feature 返回 unsupported | EA inode、bigalloc、orphan-file、inline-data write 等 feature 尚未实现 | 当前操作失败 | filesystem 仍保持可审计状态，但该能力不可用 | 3 | feature negotiation 和显式 `UnsupportedKind` |
| F-02 | journal commit 或 checkpoint 失败 | 设备写入/flush 错误 | committing 或 pending checkpoint 状态保留并 abort journal | 当前 mount 后续 sync/mutation 拒绝继续，重新挂载时依赖 recovery | 2 | 失败返回前永久 abort；保留 recovery evidence 和 pending state，禁止后续 sync 假成功 |
| F-03 | recovery-time orphan cleanup 失败 | crash 发生在 namespace commit 之后、final cleanup 之前，bad-orphan 链需要切断，或 bitmap/inode metadata、设备失败、inode 使用尚未支持的格式 | 明确的 bad-orphan 在 durable clear 后终止；不能安全降级的错误保留 orphan head 或 recovery evidence 并返回 | filesystem 不会在未持久化 cleanup 时被当作可写 root 暴露，也不会把 checksum/I/O 错误当成 stale orphan 丢弃 | 2 | legacy orphan cleanup 使用独立 journal transaction；bad-orphan clear 与普通 inode cleanup 都采用 `PreserveDuringRecovery`，checkpoint 后重载 mutable metadata state，成功后再继续；真实镜像测试覆盖 reserved/out-of-range、未分配 head、非法 next，flush-failure 测试覆盖证据保留和重试，N3 在最终执行图上补齐其余 fault/powercut 矩阵 |
| F-04 | 粗粒度 write lock 串行化慢 mutation I/O | live kext4 KVFS 层在 blocking filesystem mutation 周围持有 mount-level core write guard | 吞吐下降 | 其他 KExt4 mutation 等待；只读路径仍可共享 read guard | 4 | inode private state 已有独立 state lock；N2 根据实际 worker 和共享状态继续建立 per-group/journal/metadata-buffer 锁域；锁拆分前不宣称 mutation 并发性能 |
| F-05 | journal reservation 空间不足 | operation 的实际 metadata targets 超过空 journal 容量，或 reservation 混入另一个 lifecycle 阶段的工作 | 单次 mutation 无法容纳时在修改 metadata 前失败；writeback transaction 扩展失败时只提交完整前缀 | admission 失败保持磁盘不变；分段 writeback 的后续 transaction 若再失败，调用方可收到错误而 durable prefix 保留 | 3 | namespace 与 final eviction 分开预算；目录插入计划在事务前识别 HTree 转换/split，并把独立块 mapping 和 HTree credits 纳入同一预检；ordered writeback 在每个 live mapping run 前保证该操作及 inode publish 余量，先尝试 `reserve_more()`，容量不足则提交已 flush 的前缀、精确结算其 delalloc 后重开 transaction，只有连单次 mutation 都无法容纳时才在 admission 阶段拒绝；跨叶 truncate 在首个 metadata 写入前选择按 tree blocks/groups 估算的全树路径；由 small-journal actual-run restart、fragmented unwritten、failure/retry、rename-overwrite、balanced-split 和 preallocation-tail credit 回归测试约束 |
| F-06 | PageCache 与 filesystem core 锁序反转 | writeback 持有 core mutex 等待 mapping mutex，同时 cache miss 持有 mapping mutex 进入 backing read | 并发 buffered I/O 和 `sync()` 停止推进 | watchdog 报告 mutex deadlock，filesystem workload 无法继续 | 2 | kext4 KVFS 层不在 PageCache traversal 外层持有 core mutex；同 inode writeback 独立串行化，batch callback 在 mapping/folio mutex 释放后才进入核心；VFS/MM 后续仍需消除 tree lock 下的 backing I/O |
| F-07 | committed journal 占满可追加空间 | checkpoint 落后于 commit，head 接近 oldest tail | 新 transaction 暂时不能持久化 | mutation 等待 checkpoint progress | 3 | append 前按环形 live range 校验空间并保留一个空 block；提交路径捕获 `JournalBusy`，同步推进最老 pending checkpoint 后重试 |
| F-08 | FIEMAP 查询遇到损坏 mapping | extent tree/legacy pointer 返回非法长度或物理范围 | 当前 ioctl 返回数据错误 | 文件内容不被修改，调用方不能取得布局 | 3 | 复用 ext4 mapping 校验；KVFS operation 拒绝零进度和算术溢出，不输出未经检查的 extent |
| F-09 | eviction 与并发 `iget` 交错 | final teardown 已开始时另一路按相同 inode number 加载 | 调用方可能观察正在释放的 metadata，或建立并行 identity | stale I/O、重复释放或 transient state 串线 | 2 | KVFS cache mutex 下完成 `Live -> Freeing`；lookup/insert 在内部等待，finish 精确移除旧 Weak entry 并唤醒后重试，不向路径操作返回 `EINVAL`/`ESTALE` |
| F-10 | 存量 inode 的 extra-isize 扩展在每次脏写重复失败 | xattr 布局或 external block 资源持续不满足，但失败结论没有驻留状态 | 每次 ordinary inode dirty 重复 metadata I/O、checksum、解码、排序和布局规划 | 元数据更新吞吐下降，磁盘满场景可能被放大 | 4 | 确定布局不可行时在唯一 resident `Ext4Inode` 状态中设置 `no_expand`；成功删除 xattr 后清除，journal busy/credits 不足保持可重试 |
| F-11 | statfs 总容量模式选择错误 | context 丢失显式选项、无选项 reconfigure 重置现值，或默认模式成为 minixdf | `f_blocks` 与 Linux ext4 ABI 口径不一致 | 容量监控与应用空间判断错误 | 3 | 新挂载默认 `Bsd`，显式模式保存在唯一 `Ext4SbInfo`；无选项 reconfigure 保留现值；分别验证 minixdf 精确总块数和 bsddf overhead 结果 |

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

- 整盘 legacy direct/indirect block-map 格式当前不能挂载；只读 legacy mount 需要先把
  core mount state 拆成不可取得 mutation API 的能力类型。带 extents feature 的镜像中，
  个别 legacy inode 仍只支持受检读取；buffered write/page fault 在 reservation 前同步拒绝，
  其他 mutation 最迟在首次 metadata access 前拒绝。
- 运行态已通过 KVFS 暴露 `user.*`、`trusted.*` 和 `security.*` xattr；namespace/DAC 在
  KVFS 检查，缺少完整 LSM/capability 时 `security.*` set/remove 要求 privileged
  credential，create/replace 的四种标志组合由 KExt4 core 在同一写锁的 mutation plan
  中检查。磁盘 immutable/append-only flags 在 iget 时映射到 KVFS 并阻止 xattr mutation；
  当前尚未实现 `FS_IOC_SETFLAGS`，因此不存在需要运行时刷新这些 flags 的修改入口。
- POSIX ACL 当前只是 core 内的 opaque xattr bytes，不实现 ACL permission enforcement、
  mode 同步或 inheritance，因此 kext4 KVFS 层不把 `system.posix_acl_*` 作为普通 xattr 暴露；
  mount parser 接受 `acl` 只用于兼容 Linux 默认开启选项的正向拼写，不得改变上述权限边界。
- EA inode、oversized xattr、bigalloc、orphan-file、inline-data write、huge-file write
  block-unit accounting、encryption/casefold、direct I/O 和 mmap coherence 仍是后续工作。
- HTree SIPHASH root 可以完成格式分类，但 encryption/casefold name-key 语义尚未实现；需要名称
  哈希的 create/unlink/rename 等操作返回 `EOPNOTSUPP`。
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
- 目录 parser 是否由 `RawDirectoryEntry` 统一执行 Linux `rec_len` 解码？HTree 读写是否复用
  同一个 root decoder，且把非法磁盘 hash version、mount-time unsigned 算法和合法但未实现的
  SIPHASH 分开分类？未指定 signedness 的 RW/RO mount 是否分别持久化/保留磁盘 flags？Orlov 是否
  始终绕过 HTree default/signedness policy 而使用 signed HALF_MD4？
- xattr list 是否只借用名称，同时继续校验 value range 和 external block checksum？相同值
  set 是否在 journal/ctime 更新前返回？磁盘 immutable/append-only flags 是否在 iget 时
  映射到 KVFS，并在任何 xattr mutation 前返回 `EPERM`？inline/external 混合布局是否保存
  全部属性，普通 set/remove 是否保持当前 extra-isize，统一 ordinary inode-dirty 入口是否覆盖
  目录和非写入元数据更新，extra-isize 扩展失败是否安全回退且只缓存已经完成布局/资源判定的
  失败，而不把 journal 暂时繁忙记成永久状态？
- 每个 mutation 是否为所有可能 dirty 或 revoke 的 metadata block 预留了足够 journal credits？
- 线性目录转 HTree 后立即 split 时，extent 预检是否把两次独立 block allocation 视为两个
  最坏情况下不合并的 mapping，并在 `EXT4_INDEX_FL` 发布前预留 HTree credits？
- zero-link cleanup 是否在一个可审计事务中释放 data、external xattr block、orphan entry 和
  inode bitmap state？
- zero-link inode 是否只能由既有 VFS identity 访问，并在最后引用消失前保持 inode number、
  extent 和 xattr 有效？
- FIEMAP 是否受查询末端和 inode 格式最大文件大小双重限制，并正确区分 mapped、
  unwritten、delayed、hole 与截断结果的 `LAST`？
- MAP_SHARED 写缺页是否先通过 `page_mkwrite()` 建立 delayed reservation，再发布 writable
  PTE；该过程是否持有 address-space invalidate shared lock 与 folio lock，而不取得 inode
  exclusive data lock，使非 `SYNC` FIEMAP 不漏报脏 hole？
- 运行态 `iget` 是否只由 KVFS cache initializer 解码 private state，且 `New/Freeing` 等待重试？
- namespace mutation 是否传入既有 VFS victim/moved/replaced private state，而不是 core 按编号重载？
- 新增 ext4-private transient 字段是否放在 VFS inode 组合持有的 `Ext4Inode`，而不是另建 wrapper
  snapshot、mount-wide inode-number map 或通用状态容器？
- 文档是否区分了 core support、live KVFS syscall exposure 和路线图能力？
- statfs 模式是否只改变 `blocks`，并保持 free/available/reserved/delalloc accounting 共享？
