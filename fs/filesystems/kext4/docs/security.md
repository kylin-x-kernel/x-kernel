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
（含直接持有的 `MappingPages` storage）和文件生命周期回调。KExt4 核心不直接解引用用户态指针。
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

## 读取缓冲与失败发布边界

整块 initialized 文件读取复用调用者的 `&mut [u8]`，不引入 unsafe、用户指针解引用或
新的 DMA 所有权。块 trait 对直读与部分块暂存缓冲都接受任意字节起点；后端及其 DMA 能力层
负责设备地址对齐、连续性、映射与缓存维护。文件系统不以常量或 `Vec<u8>` 的偶然分配对齐
代替设备契约，也不把缓冲要求转嫁给 `read_at` 调用者。
这个约定不取消块长度/范围校验，也不承诺设备 I/O 失败原子性。
hole/unwritten 分支不能读取物理旧数据，部分块和 EOF 不能把
请求外字节复制给调用者。

块后端可能先修改目标前缀再返回错误，因此 `read_at` 不保证失败缓冲保持旧值，也不返回
可发布的成功前缀。调用者必须传播错误，不能置 uptodate 或把失败内容复制到读迭代器。
单页缓存失败保留不可读的非 uptodate folio；批量预读失败不提交任何新页，未消费 reservation
由 PageCache 回收。完整重试成功后才允许发布；错误路径的缓冲内容不是有效文件数据。
`vfs/read_tests.rs` 以真实 ext4 + KVFS + PageCache 链路验证上述隔离，并检查重试后的页未被
标脏、17 字节 EOF 尾部补零。失败读后部分写入的测试还验证完整重新读页先于写入复制，
只把成功写入后的完整数据标脏，不发布此前失败读污染的前缀。该验证不声称设备 I/O 具备失败原子性。

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
- A speculative prealloc space exists only in memory: it is not marked in the block bitmap, the group
  descriptor or the inode, so discarding it at any moment leaves no disk residue; it reaches the bitmap only
  when consumed, through the normal allocation path, and inside that same critical section both indexes are
  detached before the consumed span is marked, so no window separates exclusion from the disk mark.
  Conversely, an entry kept on one side alone protects nothing and must not be treated as available space -
  the inode-side logical range and the group-side twin entry must be added and removed together inside one
  allocator critical section, and `preallocated_blocks` must always equal the sum of the group-side span
  lengths (verified after every change in debug builds).
- group 侧孪生条目持有 owner inode 的强引用，所以 eviction 必须无条件丢弃该文件的 space；漏丢
  会让条目与其强引用一起活过 inode 对象。
- mount 生命周期内的 `MountedJournal` 是生产路径唯一 journal identity，同时拥有磁盘 mapping、
  活跃 superblock 状态、transaction engine 和 FIFO checkpoint queue。同一个 runtime
  transaction 必须按 Running → Committing → Checkpoint → Finished 迁移；队列只保存该对象，
  持久化证据属于其 Checkpoint phase，不复制 payload 或携带可替换的 coordinator 引用。
- 提交请求在事务锁下先关闭目标 running transaction 的新 handle 准入，再释放状态锁等待
  活跃 handle 退出；stop 和 abort 必须唤醒等待者。不能持有自身 handle 或退出方需要的锁
  等待提交，也不能嵌套 begin 等待自己所在事务解封。重复持久化请求必须识别 pending/finished
  目标，不能重复 append 或退休；无效未来目标必须失败，不能等待不相关事务。
- 可变事务的 ordered-data flush 只能确认 barrier 开始前快照的依赖代次；较旧 barrier
  完成不能倒退确认水位，失败不能清除依赖。冻结后不再接受新依赖，commit 在日志写入前
  满足其完整 ordered-data 依赖。
- journal 持久化 execution mutex 串行 append、checkpoint、ring/tail 与 recovery flag I/O；
  短 transaction/state/buffer guard 不得跨设备 I/O。home 和 tail 两次持久化成功前不退休，
  失败须在交出执行权前 abort。首次 feature 更新和 PreserveDuringRecovery 同样参加该协议；
  replay/reset 仍限于 mount 身份发布前的独占恢复。
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
- 释放 bitmap 位之前，分配器必须按块组和 freeing transaction ID 登记待复用区间；
  区间、统计与 bitmap/cache 发布归同一 allocator owner、受同一锁保护。JBD2 不保存物理
  空间记录；文件系统胶水仅在提交持久化成功后、释放 journal 锁后通知分配器解除该事务的
  保护。handle stop、freeze、缓存重建或失败都不能解除；无需等待 home checkpoint。
  连续区间按组有界处理，跨段失败必须走 mutation-error abort。分配同时排除 prealloc 与
  待复用区间；surplus 登记不得通过 bitmap-free 采样承诺不可复用块。提交失败保留记录并
  abort，不能向后续分配开放这些块。
- 回收阶段可共享事务，但每个因同步/额度压力提交的前缀必须保持 orphan 与剩余映射一致；
  每个回收阶段在 handle 创建成功后独立禁止 dying inode 的 extra-isize 自动扩展，不能
  依赖上一阶段标志，也不能在清理 xattr 后重新建立 EA block；准入失败不改变该标志。
  运行态回收仅裁剪最右侧叶并沿路径回收空节点，不分配或重建子树；操作前失效映射缓存。
  批次超过单事务容量时必须先有界缩批，容量判断共用 journal 契约；其他 NoSpace 或
  提交错误不能被吞掉并当成缩批重试。
- 有界物理空间准入只能在 mount mutation 排他下、打开 handle 前进行，不把 metadata
  credits 当成物理块需求。空间不足可先丢弃可选预分配，再提交可解禁的 freeing transaction；
  不得持有 active handle 等待自身提交，也不得重试已经部分发布 metadata 的 namespace 操作。
  writeback 转换以实际叶项变化与分裂布局计树块需求，零增长操作不能为固定预算推动提交。
  有待复用块时逐映射关闭 handle 再准入，停止 handle 本身不建立持久化边界。
  准入或 handle completion 隐式提交成功后，必须与显式 Restart 一样确认完成前缀、
  释放该范围预留。后续错误/abort 不得把已提交前缀重新算作未完成；失败的提交不得
  推进前缀。完成依据来自 journal 原有事务生命周期，并遵循事务 ID 回绕规则。
  现有 admission 失败不 abort、mutation 后失败 abort 并发布只读状态的边界不变。
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

运行态 KVFS `SuperBlock` 把无状态 `Ext4SuperOperations` 与唯一 `Ext4Mount`（内部 `RwLock<Ext4SbInfo>`）
分别作为 `s_op`/`s_fs_info` 保存；`s_op` 是所有挂载共享的真正静态表，private object 由
superblock 独占且只能借用恢复，不存在承载 mount identity 的 operation wrapper 或可逃逸的
typed owner。所有 inode mapping 也共享静态 `Ext4AddressSpaceOperations`，callback 只从
`mapping->host` 借用唯一 `Ext4Inode`；共享写 fault 的准备由 `Ext4Inode` 的
`FileMappingOperations` 完成。仍需联合修改
superblock/group metadata 的 mutation 由挂载级 write guard 串行化；只读 core 操作共享进入，
delalloc range 与 mount aggregate 则在 shared guard 下由独立 reservation mutex 联合更新。KVFS
cache 合并并发 `iget` 并封闭 `New/Freeing`；ext4 private metadata 与 transient state 在 inode
state mutex 下更新，KExt4 不再取得另一把 inode-cache lock。内部 JBD2 和 metadata-buffer 状态仍会记录
transaction ownership。共享 buffer 原子修改在 slot mutex 下完成，失败不替换旧字节；同一事务
的不同写者也不能分别读旧快照后整块覆盖。inode-table 的发布只合并目标 entry，保留
邻居 inode 的最新数据；同一 inode 修改仍要求调用者串行。writeback、VFS truncate 两阶段、
最后写者预分配清理和 final eviction 共同由 Ext4Inode mutex 串行化；它先于 mount 获取，
不得在 mount 或 inode-state guard 内反向等待。writeback 扫描 PageCache 时不持有 core mutex，batch writer 只在 PageCache 释放
mapping/folio mutex 后进入核心。journal execution mutex 位于 mount 之后；handle stop 不取得它。
容量 preflight 与 begin 仍由同一次 mount mutation 排他保证，尚不是独立 credit reservation，
不得据此放开全部 metadata mutation。后续引入细粒度锁时，必须保持该跨层边界，以及 buffer
ownership、journal handle、allocator bitmap 和 inode metadata 之间的顺序约束。
FIEMAP 由 VFS inode shared lock 稳定单 inode mapping；core read lock 和 delayed-block
mutex 只分别短暂取得，并在安全 writer 访问用户页前释放。不得把用户内存访问放进挂载级
core lock 或 delayed-block mutex。Buffered write 和 shared write fault 都必须在 inode
exclusive data lock 下先把 hole reservation 发布到同一个 delayed set，不能以 dirty-folio
扫描替代 filesystem mapping prepare。
speculative prealloc space 的 per-inode map 锁是 allocator 锁之下的模块私有叶锁，刻意不使用
inode state 锁：消费与 discard 都在持 allocator 锁的临界区内取放它，`update` 闭包内不再取得
任何锁，group 侧 discard 锁 foreign owner 的 map 也按同一顺序取，因此 prealloc 操作不会与
inode-state 使用者构成 ABBA。ENOSPC 重试阀自带 allocator 锁，只能在扫描的锁段之外调用。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 损坏镜像中的 xattr entry name 或 value range 被 name-only list 路径当成合法属性 | 中 | 恶意或损坏镜像编码 NUL、越界/重叠 value 或无效 external block | `decode_xattr_entries()` 拒绝 NUL 和畸形 entry；name-only 遍历仍校验 value range、block header 与 checksum，并有回归测试 |
| T-02 | external xattr block 在 unlink 或 rename overwrite 后泄漏，或 inode 继续引用已释放 block | 中 | zero-link victim 的 `i_file_acl != 0` | zero-link eviction 先释放或递减 EA block refcount，清 `i_file_acl`，更新 `i_blocks`，再释放 inode |
| T-03 | 运行态 unlink 过早释放仍被打开 fd 引用的 inode | 中 | KExt4 运行态后端上 unlink 一个仍打开的 inode | namespace transaction 只持久化 nlink/orphan；open-file 持有唯一 VFS inode 及其 ext4 private state，最后 VFS 引用销毁才进入 superblock final eviction |
| T-04 | journal credits 估算不足或按 data blocks 过度估算，导致 metadata update 在事务中途失败或被错误拒绝 | 中 | namespace remove、HTree 转换/split、writeback、truncate、preallocation discard 或 final eviction 修改 orphan、xattr、extent 和 inode metadata | namespace 插入预检显式记录 HTree 路径，转换后立即 split 按两个独立单块 extent 预估并在 inode flag 发布前加入 HTree credits；namespace removal 只覆盖 dirent、nlink 和 orphan metadata，不包含后续 final eviction；ordered writeback 固定为 path-local 算法，由 live cursor 按实际 unwritten conversion 和 hole allocation run 续订 handle，达到 transaction 上限时提交完整前缀并重启；legacy final eviction 和 extent truncate 按实际 tree blocks、需要 revoke 的旧 tree blocks 与 affected groups 计算，目录/block-mapped symlink 数据块释放按释放块数逐一计入 revoke credits；victim 带 external xattr 时 final eviction 预算包含 EA 清理 |
| T-05 | 恶意 extent 或 bitmap metadata 导致释放不属于该 inode 的 block | 高 | 损坏镜像把 extent/xattr block 指向 system zone 或非法 group | release 或 mutation 前执行 block ownership、system-zone、bitmap 和 checksum 校验 |
| T-06 | 设备写入/flush 失败让部分 metadata 可见 | 中 | commit、replay、checkpoint、显式 sync 或 xattr update 期间设备失败 | journal abort 保留 recovery state 或 pending checkpoint；后续 sync/mutation 返回 aborted，不跨 syscall 回滚内存修改；测试覆盖 explicit-sync commit、checkpoint、xattr 和 replay failure |
| T-07 | clean journal 上残留 orphan，mount 永久返回 `NeedsRecovery` | 中 | namespace transaction 已 checkpoint，但 final inode eviction 尚未发生 | 显式 recovery 无论 journal 是否需要 replay 都遍历 legacy list 与 orphan file；clean 分支以 `PreserveDuringRecovery` 建立 recovery evidence，逐个同步 commit/checkpoint，确认 journal start 清零且 orphan file 为空后才清除 recovery features；zero-link entry 复用 journaled final-eviction 路径 |
| T-08 | free-block aggregate、delalloc 区间与 reservation aggregate 漂移导致过量预留 | 中 | VFS/ext4 两侧分别更新，或逐块 set 与 mount counter 只更新一侧 | block mutation 同时发布 superblock 与 group descriptor；ext4 range API 在独立 reservation mutex 下联合更新 inode interval、`i_reserved_data_blocks` 等价计数和 mount aggregate，admission/statfs 直接使用该 aggregate |
| T-09 | 新建 inode 固定为 root，绕过调用者 owner 语义 | 高 | callback 丢弃 credential 或 ext4 constructor 隐式填入 UID/GID 0 | create/mkdir/mknod/symlink callback 使用 `inode_init_owner()`，显式 `uid`、`gid` 随同 namei transaction 持久化 |
| T-10 | journal head 追上 tail 并覆盖尚未 checkpoint 的 commit | 高 | 多个 committed transaction 占满环形日志，追加仍继续写入 | 依据 oldest tail/current head 计算 live 空间，始终保留一个空 block；空间不足时先 checkpoint 足够释放空间的连续前缀再重试，不发出覆盖写；真实 ext4 镜像测试覆盖双 transaction tail 推进 |
| T-11 | 损坏 extent 或超大 FIEMAP 范围造成算术溢出、错误物理地址或无界遍历 | 高 | disk mapping 长度越过格式容量，或字节/块换算未检查 | ext4 算法暴露 Linux 对等的 inode 格式相关最大字节数；KVFS operation 对每次加法、乘法和输出字段做 checked 校验，并把 mapping 截断到请求与格式边界 |
| T-12 | 磁盘 inode 的 immutable/append-only 状态在 VFS 接入中丢失，导致 xattr 被修改 | 中 | iget 构造 KVFS inode 时总是使用空 `NodeFlags` | ext4 以语义方法暴露 `EXT4_IMMUTABLE_FL/EXT4_APPEND_FL`；kext4 KVFS 层在发布 VFS inode identity 时映射为 KVFS flags，由通用 xattr 权限层在 mutation 前返回 `EPERM` |
| T-13 | 同一 inode 出现 snapshot 分叉，或 inode number reuse 继承旧 transient state | 中 | KVFS 初始化/释放期间并发 `iget`，namei 按编号重载 live child，或 orphan removal 为 resident 前驱解码临时对象 | KVFS-wide `(SuperBlock, ino)` table 的 `New/Live/Freeing` 是唯一 identity 状态；`New/Freeing` 等待并重试；kext4 向 unlink/rmdir/rename 传入既有 private component；legacy orphan next 只读写 journaled inode-table bytes；KExt4 无 resident cache；reuse 只能在 `Freeing` 完成并删除旧 slot 后发生 |
| T-14 | shrink 后重新增长暴露旧 PageCache 数据或沿用失效的 extent proof | 高 | ext4 backing prepare 提前发布 `i_size` 使 `truncate_setsize()` 跳过清理，或把 atomic 单值发布误当成尺寸事务并移除映射失效互斥 | regular-file metadata publish 只更新 `i_disksize`；VFS 与 ext4 共享唯一 `i_size`，由 VFS 在 PageCache 顺序点以 release store 发布，EOF 读取使用 acquire load；尺寸写入持 inode-state 写锁，shrink 清除 proof 后才发布新 EOF，快照及 extent proof 在同一锁内采样；inode data/invalidation 锁继续约束 PageCache/PTE 顺序，atomic 不提供失效互斥；通用属性共享、并发 shrink/proof、core prepare 及 KVFS shrink/regrow、partial-grow 回归测试共同约束该职责边界 |
| T-15 | 扩大 `i_extra_isize` 时覆盖、丢失 xattr，生成 e2fsck 不接受的 external EA block，或在磁盘满时拒绝本可完成的更新 | 高 | xattr syscall 错误地同时改变 extra-isize、普通 inode dirty 路径漏掉扩展，新 extra fields 与旧 inline 区间重叠，external entry/block hash 留零，错误地 COW 内容未变的共享 block，或布局阶段的空闲块在 apply 前被占用 | 普通 set/remove 固定使用当前 extra-isize，统一 ordinary inode-dirty 入口再按 want/min/current 扩展；迁移采用 Linux 风格 entry-by-entry 选择且固定 `system.data`；内容未变的 external block 直接复用，需要新 block 的布局在 apply 前预分配并把 `NoSpace` 降级为 `no_expand`；同一 journal transaction 重写 external block、生成 Linux-compatible `e_hash`/`h_hash`、清理旧 inline 区并更新 inode checksum；单元测试覆盖 deferred expansion、目录 inode dirty、磁盘满共享 external 复用、迁移选择与独立固定 hash 向量 |
| T-16 | 损坏 orphan entry 让挂载恢复失败、循环，或访问未分配/special inode | 中 | `s_last_orphan` 或 orphan-file slot 越界、指向 allocation bit 为零或内部 backing inode，已加载 inode 带不可截断的链接类型，或 legacy `i_dtime` 指向非法 next | 统一 `orphan_iget` 在 inode-table decode 前验证编号、special-inode 边界和带 checksum 的 bitmap，加载后验证格式与可清理性；legacy bad chain 以 `PreserveDuringRecovery` transaction 持久化清零，非法 orphan-file entry 跳过并继续处理后续有效槽位，最终保留恢复证据并失败；bitmap I/O/checksum、inode decode 和合法但未支持的 cleanup 不被降级 |
| T-17 | `minixdf`/`bsddf` 改变 free-space 事实源或错误扣除 metadata overhead | 中 | 两种模式分别实现整套 statfs 公式，或 VFS integration 直接修改 free counters | typed mode 只选择 `blocks_count - overhead` 或原始 `blocks_count`；free/available/inode 统计走共享代码，Linux-image 回归测试验证两者除总块数外一致 |
| T-18 | 128-byte inode 对外暴露不存在的纳秒精度，或新建 inode 丢失纳秒/扩展 epoch 并在 2038 年后回绕 | 中 | 只在 inode-table encode 时截断、VFS 比较原始高精度时间，或初始化结构只保存 base `u32` 秒 | KExt4 按 inode size 返回共享的 `TimestampLimits`，由唯一 KVFS `SuperBlock` 持有；`VfsInode::current_time()` 在比较、callback 和 publication 前只截断一次；新建 inode 保存完整 `Ext4Timestamp` 并通过 base/extra encoder 写盘，128-byte 格式在边界钳制，256-byte 格式保留纳秒和 epoch；回归测试覆盖两种 inode size 的能力选择、初始化编码和 resident publication |
| T-19 | 合法 SIPHASH HTree 被误报损坏 | 中 | root `hash_version=6` 进入目录写路径 | 共享 HTree root decoder 只接受磁盘版本 `0/1/2/6`，HTree signedness 来自 mount state；SIPHASH 在需要密钥的 hash 边界返回 `Unsupported`，相关事务不能提交 metadata mutation |
| T-20 | 未标 signedness 的旧 HTree 镜像跨 Linux 挂载后名称不可见，或 Orlov 目录放置漂移 | 高 | 磁盘两种 signedness flag 都为空时按 signed 计算，或 Orlov 复用 HTree default/unsigned policy | mount state 按 Linux unsigned-char 语义生成 `hash_unsigned`，RW 持久化 unsigned flag、RO 保持磁盘不变；HTree 只经 mount policy hash，Orlov 独立固定 signed HALF_MD4 + seed；高位字节名称回归测试区分 signed/unsigned |
| T-21 | The two indexes of a speculative prealloc space drift apart: a leftover entry on one side protects no block yet is treated as an available span, or a group twin outlives its owner inode | Medium | The inode side and the group side change in different critical sections, or close / writeback settlement / eviction drops only one side | Both sides change only together inside one allocator critical section; `preallocated_blocks` is conserved against the sum of the group-side span lengths and checked by a debug assertion; each twin holds a strong owner reference; close (when no delalloc reservation remains), writeback settlement (when reservations reach zero and no writable open remains, with an empty reclaim skipped via the inode-side emptiness hint) and eviction (unconditional, never short-circuited on the inode-side hint so a drifted pair cannot leave a twin behind) all discard; regression tests cover the close, evict and shrink reclaim paths and dual-index consistency |
| T-22 | ENOSPC 重试阀在"聚合非零但选不出块"状态下无界重扫，或用重试掩盖位图损坏 | 中 | free 聚合把未消费 space 计为空闲；损坏组被每轮重扫再次命中 | 搜索次数上界固定为 `PREALLOC_DISCARD_SEARCH_LIMIT`（3 次，discard 只可能出现在第一次之后）；discard 返回 0 立即结束；重试只由健康组的无空间结论触发，几何/描述符预检跳过不置位；扫描中首个 `InvalidBlockBitmap` 被记住并优先于 `NoSpace` 返回，但不封闭健康组的回收重试 |

## 故障模式与影响分析（FMEA）

Namespace removal 的共享 eviction preflight 在首个 metadata mutation 前校验 inode 数据
格式。Orphan file 在挂载/replay 后完成校验和初始化；运行时由 `OrphanInfo` 在挂载级独占访问下
维护槽位和 free-entry 计数，metadata publication 成功后才更新计数，失败由 journal abort 路径
保留磁盘恢复证据。多硬链接且不会归零的普通 inode 不分配 orphan entry。内核组件测试使用合成
metadata 覆盖 unlink、rmdir、覆盖 rename 和乱序 final eviction；镜像互操作与 e2fsck 属于外部 harness。

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 对 Linux ext4 有效 feature 返回 unsupported | EA inode、bigalloc、inline-data write 等 feature 尚未实现 | 当前操作失败 | filesystem 仍保持可审计状态，但该能力不可用 | 3 | feature negotiation 和显式 `UnsupportedKind` |
| F-02 | journal commit 或 checkpoint 失败 | 设备写入/flush 错误 | committing 或 pending checkpoint 状态保留并 abort journal | 当前 mount 后续 sync/mutation 拒绝继续，重新挂载时依赖 recovery | 2 | 失败返回前永久 abort；保留 recovery evidence 和 pending state，禁止后续 sync 假成功 |
| F-03 | recovery-time orphan cleanup 失败 | crash 发生在 namespace commit 之后、final cleanup 之前，bad legacy chain 需要切断，或 orphan-file/bitmap/inode metadata、设备失败、inode 使用尚未支持的格式 | 明确的 legacy bad-orphan 在 durable clear 后终止；不能安全降级的错误保留 orphan entry 或 recovery evidence 并返回 | filesystem 不会在未持久化 cleanup 时被当作可写 root 暴露，也不会把 checksum/I/O 错误当成 stale orphan 丢弃 | 2 | orphan cleanup 使用独立 journal transaction；bad legacy clear 与普通 inode cleanup 都采用 `PreserveDuringRecovery`，checkpoint 后只重载 mutable superblock/group state 并增量维护 orphan-file free count，成功后再继续；N3 在最终执行图上补齐 fault/powercut 矩阵 |
| F-04 | 粗粒度 write lock 串行化慢 mutation I/O | live kext4 KVFS 层在 blocking filesystem mutation 周围持有 mount-level core write guard | 吞吐下降 | 其他 KExt4 mutation 等待；只读路径仍可共享 read guard | 4 | inode private state 已有独立 state lock；N2 根据实际 worker 和共享状态继续建立 per-group/journal/metadata-buffer 锁域；锁拆分前不宣称 mutation 并发性能 |
| F-05 | journal reservation 空间不足 | operation 的实际 metadata targets 超过空 journal 容量，或 reservation 混入另一个 lifecycle 阶段的工作 | 单次 mutation 无法容纳时在修改 metadata 前失败；writeback transaction 扩展失败时只提交完整前缀 | admission 失败保持磁盘不变；分段 writeback 的后续 transaction 若再失败，调用方可收到错误而 durable prefix 保留 | 3 | namespace 与 final eviction 分开预算；目录插入在事务前检查磁盘容量，额度按当前 extent 深度的结构上界预留，仅可能转换的单块目录计入第二次 append；ordered writeback 在每个 live mapping run 前保证该操作及 inode publish 余量，先尝试 `reserve_more()`，容量不足则提交已 flush 的前缀、精确结算其 delalloc 后重开 transaction，只有连单次 mutation 都无法容纳时才在 admission 阶段拒绝；跨叶 truncate 在首个 metadata 写入前选择按 tree blocks/groups 估算的全树路径；由 small-journal actual-run restart、fragmented unwritten、failure/retry、rename-overwrite、balanced-split 和 preallocation-tail credit 回归测试约束 |
| F-06 | PageCache 与 filesystem core 锁序反转 | writeback 持有 core mutex 等待 mapping mutex，同时 cache miss 持有 mapping mutex 进入 backing read | 并发 buffered I/O 和 `sync()` 停止推进 | watchdog 报告 mutex deadlock，filesystem workload 无法继续 | 2 | kext4 KVFS 层不在 PageCache traversal 外层持有 core mutex；同 inode writeback 独立串行化，batch callback 在 mapping/folio mutex 释放后才进入核心；VFS/MM 后续仍需消除 tree lock 下的 backing I/O |
| F-07 | committed journal 占满可追加空间 | checkpoint 落后于 commit，head 接近 oldest tail | 新 transaction 暂时不能持久化 | mutation 等待 checkpoint progress | 3 | append 前按环形 live range 校验空间并保留一个空 block；提交路径捕获 `JournalBusy`，同步处理足够释放空间的 pending checkpoint 前缀后重试 |
| F-08 | FIEMAP 查询遇到损坏 mapping | extent tree/legacy pointer 返回非法长度或物理范围 | 当前 ioctl 返回数据错误 | 文件内容不被修改，调用方不能取得布局 | 3 | 复用 ext4 mapping 校验；KVFS operation 拒绝零进度和算术溢出，不输出未经检查的 extent |
| F-09 | eviction 与并发 `iget` 交错 | final teardown 已开始时另一路按相同 inode number 加载 | 调用方可能观察正在释放的 metadata，或建立并行 identity | stale I/O、重复释放或 transient state 串线 | 2 | KVFS cache mutex 下完成 `Live -> Freeing`；lookup/insert 在内部等待，finish 精确移除旧 Weak entry 并唤醒后重试，不向路径操作返回 `EINVAL`/`ESTALE` |
| F-10 | 存量 inode 的 extra-isize 扩展在每次脏写重复失败 | xattr 布局或 external block 资源持续不满足，但失败结论没有驻留状态 | 每次 ordinary inode dirty 重复 metadata I/O、checksum、解码、排序和布局规划 | 元数据更新吞吐下降，磁盘满场景可能被放大 | 4 | 确定布局不可行时在唯一 resident `Ext4Inode` 状态中设置 `no_expand`；成功删除 xattr 后清除，journal busy/credits 不足保持可重试 |
| F-11 | statfs 总容量模式选择错误 | context 丢失显式选项、无选项 reconfigure 重置现值，或默认模式成为 minixdf | `f_blocks` 与 Linux ext4 ABI 口径不一致 | 容量监控与应用空间判断错误 | 3 | 新挂载默认 `Bsd`，显式模式保存在唯一 `Ext4SbInfo`；无选项 reconfigure 保留现值；分别验证 minixdf 精确总块数和 bsddf overhead 结果 |
| F-12 | A speculative prealloc space is not reclaimed with its owner's lifecycle | Close was gated by a delalloc reservation and eviction did not cover the gap, or only one of the two indexes was changed | Blocks are counted in `preallocated_blocks` without an owner, and the group scan keeps excluding those bits | Shows up as "the free aggregate is nonzero but allocation fails"; the ENOSPC retry valve's discard bounds it, and a group twin holds a strong reference so a missed drop would keep a dead inode alive | 2 | Both sides change inside one allocator critical section; the conservation debug assertion; eviction discards unconditionally; close drops only when no reservation remains, and otherwise writeback settlement (reservations zero and no writable open) or eviction reclaims; a settlement racing the writer-count decrement keeps the spaces for the next settlement or eviction; retries are bounded and stop when discard returns 0; close/evict/shrink/settlement regression tests assert both indexes and the counter return to zero |

## 故障管理

KExt4 使用 `Ext4Result` 和 typed `Ext4Error` 表达 corruption、unsupported format、
out-of-bounds metadata、checksum mismatch、journal credit exhaustion 和 device error。
日志化修改路径把普通失败前移到首次 metadata access 前；多个 handle 的空失败不会影响同一
running transaction 中其他修改。metadata/revoke 已发布后出现普通错误、设备/checksum/状态机错误或
handle accounting 错误时会 permanent abort journal，不执行跨 operation 的内存回滚。
数据 I/O 成功只记录持久化依赖，不标记 handle 已发布 metadata；metadata access 前的普通失败
保留依赖并允许重试，例如 unwritten extent 转换因树块分配不足返回 `NoSpace`。
发布 metadata 的 ordered writeback 在设备 I/O 完成后必须设置事务依赖；依赖在 freeze 后保留，commit 在
任何日志/恢复证据写入前执行一次设备 flush。flush 失败不能清除依赖，也不能进入
checkpoint phase，必须 abort journal 并向同步调用方报错。此路径仍受 mount 写锁串行保护；
容量 restart 的显式 flush 成功后才能确认当前全部数据依赖已满足。running/committing 两种
`flush_ordered_data` 入口内部先 barrier 后清除，不暴露独立清除方法，也不持有事务内部锁执行 I/O。
data-only 的已映射覆盖写不依赖新 metadata，持久化由 inode sync 的同设备 journal flush 或直接
flush、filesystem sync 的最终 flush 提供，不承诺先于无关事务的 commit record 持久化。

Checkpoint 批处理必须保持 FIFO frozen image 写回顺序：选中连续前缀的全部 home-block
写入成功并 flush 之后，才允许一次持久化日志尾部更新；在尾部 flush 及 recovery 状态处理成功前，不移除
任何批内事务或回收日志空间。写回中途失败或任一道 flush 失败都保留整批 pending work
并 abort，禁止在当前 mount 重试已消费的缓存快照；重新挂载依赖保留的日志恢复。
空间不足时，按待提交事务的实际日志块数加下一笔 transaction credit 上限的最坏编码开销
选择前缀，目标封顶为 ring 容量减一，始终保留一个空块。`usable_log_capacity()` 是
预留规则的单一来源，append 准入、过大事务拒绝与目标封顶共用，避免全排空后仍无法
满足目标的语义漂移。可选预算不能阻塞可容纳的当前
append；计算使用与日志写入相同的 descriptor/revoke 格式和 checked 算术。完整的下一笔
预算覆盖共用 credit 池的任意 metadata/revoke 混合，证明见设计文档；ring 容量封顶可能
削减可选预算，不能据此承诺下一笔免于再次回收。剩余事务及其 policy 不移除。tail 指向 `next_oldest`，journal 必须保持 active，ext4 recovery feature
不得清除，也不能失效仍有 frozen checkpoint image 的缓存。前缀退休先按涉及的 block key 定向回收
clean、无 pin 且无后续 ownership 的 entry；后缀或 committing transaction 的缓存不能释放。
一般缓存清理不以 checkpoint 批次计数触发。`MetadataBlockCache` 从驻留 block payload
是否超过 4 MiB 软保留额度计算需求；`Ext4Mount` 的读写 guard 在访问结束、仍持有 mount
保护时最多推进 64 个条目的增量扫描。只读访问无需日志压力即可推进回收。
每轮固定结束 key，新增高地址不能推迟回绕；dirty/pin/ownership 和 frozen image 不可淘汰，
预算限制检查数而非成功淘汰数。命中在已有索引锁内设置一位复用标记，首次插入不置位；
扫描清位并给近期复用条目一次保留机会，仍消耗检查预算，不因未淘汰而额外扫描。
该位不授予淘汰权限，也不替代 pin/ownership/frozen image 校验；checkpoint 定向回收
及显式全回收忽略访问位，避免热度阻止明确清理。持续复用可能使软额度长期超出。
无需求时仅 Relaxed 读取缓存自己的 `AtomicBool` 提示，
不获取 cache mutex；提示在每次索引增删/失效的同一 cache 临界区内从实际长度更新，
避免旧扫描覆盖新插入的需求。它不发布 buffer 数据或授予淘汰权限，并发旧值只允许
推迟维护或触发一次多余复核。真正的需求复核、游标和资格检查在同一 cache mutex 下进行；
不持有该锁执行设备 I/O。普通缓存 read/insert 不淘汰；不可变 buffer 同时 pin 字节和
槽位，allocator 持有预取 pins 至临界区结束，即使 bytes 被新发布替换，也不能回收并
重建该槽位。forget/revoke 后的移除同样在 index mutex 下检查是否另有槽位引用。
mount guard 的维护入口暂时保留，但上述普通回收正确性不依赖 mount 排他。
整体 invalidate 仍要求独占恢复/生命周期边界，不得与活跃 pins 并行。
显式全回收和整个缓存失效重置游标，syncfs 仍排空并全局回收。
没有周期全扫或 checkpoint 次数状态。软保留额度不包括索引/frozen image 开销，不能保证
硬内存上界：pin/事务持有、单次访问的大量插入或新增速度超过扫描速度都可能造成超额；
没有后续访问也没有自动进度。尚无通用 shrinker、容量准入或内存压力反馈。
空间目标限制回收完成条件，但 mount 写锁内的 I/O 仍可能较长，
该策略不保证设备故障下的延迟上界。
生产 checkpoint entry 必须先取得 persisted evidence 再在事务锁内入队；缺少证据属于
内部不变量破坏，不能按仍在提交的 `JournalBusy` 状态跳过。写回前须确认每笔事务的
policy 记录存在。`PreserveDuringRecovery` 追加后在 mount 写锁下同步排空，若批内存在
该策略，它必为最后一笔，整批保留 recovery evidence；不得允许后续普通事务在排空前
覆盖批末策略。恢复工作全部完成后才可执行最终 recovery feature 清除。

Commit/checkpoint failure 会保留 committing 或 pending work 并永久 abort journal，避免静默
丢失尚未落盘的 metadata；后续 sync 和 mutation 都观察 `JournalAborted`，不能把内存中的
transaction 当作 durable。journal tail 只能越过已经完成 home-block 持久化的连续事务
前缀；只有覆盖全部 pending commit 的批次完成后才清除 journal start 和 ext4 recovery
evidence。当前 inode sync
采用提交整个 running transaction 的保守语义。显式 recovery 同样只有在 legacy list 与
orphan-file cleanup 全部完成 commit/checkpoint、确认 journal start 为零且 orphan file 为空，
并 flush 最终 superblock 状态之后才能返回成功；在 cleanup durable 之前失败会保留 orphan entry 或 recovery feature
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
- EA inode、oversized xattr、bigalloc、inline-data write、huge-file write
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
- Birth and consume of speculative prealloc spaces are wired into the writeback path: the Hole branch consumes
  a registered space first and, only on a miss, allocates and registers the surplus inside the same allocator
  lock section; the reclaim side (close, eviction, shrink, writeback settlement), group-scan exclusion and the
  ENOSPC retry valve are all live. The `preallocated_blocks` conservation assertion is checked after every
  change in debug builds, so the two sides desynchronizing is exposed immediately.

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
- 目录插入需要分配新块时，link/rename 是否在发布 metadata 前预留 bitmap/group/superblock
  增长额度，而不是依赖只存在于 create 路径的 child inode allocation 预算？普通文件
  创建是否避免重复申请父目录增长额度，同时保留 mkdir/长 symlink 的独立 child 块预留？
- 文档是否区分了 core support、live KVFS syscall exposure 和路线图能力？
- statfs 模式是否只改变 `blocks`，并保持 free/available/reserved/delalloc accounting 共享？


### Directory growth extent-credit boundary

Namespace insertion reserves an operation bound independent of directory contents or
predicted insertion paths. The extent layer bounds new blocks and old external rewrites
using the validated current depth: existing external blocks are reused, each level can
add one sibling, and inline spill/root growth are included. Only a single-block linear
directory eligible for index conversion reserves a second append, allowing one level
of depth growth between appends. HTree dispatch and credits share the feature-and-inode
index predicate. Allocation/create and release/revoke keep distinct credit definitions.
Capacity validation remains ahead of metadata publication because namespace mutations
have no operation-local rollback. No plan object or cached growth-credit field is retained.

An oversized admission returns `NoSpace` without aborting or publishing metadata. A full
running transaction waits for active handles, then commits before the caller retries its
reservation; exact-capacity admission is allowed. Waiters observe the existing transaction
state and share its notification queue. Handle stop and abort notify outside journal locks.
The mount mutation lock serializes complete filesystem mutations. Admission checks the
existing active-handle count and rejects nested handles before any setup or wait, so a
caller cannot wait for its own handle while holding that lock. Independent journal-core
users retain the wait/stop protocol; handle stop requires no mount lock.

Credit exhaustion before metadata publication returns `InsufficientJournalCredits`, maps
to `ENOSPC`, and leaves the journal writable. Exhaustion after publication violates mutation
prevalidation, becomes `InvalidJournalTransaction`, aborts the journal, and maps to `EIO`. The existing
abort state rejects subsequent mutation with `EROFS` before feature or superblock setup;
the mount write guard publishes `RDONLY` to the existing VFS superblock before releasing
mutation protection. Writable remount validates the same abort state before applying
options; buffered-write admission and delayed reservation also reject abort independently
of VFS flag publication, before dirtying the page cache. The guard borrows the caller's
superblock; the mount retains no additional reference or
writable-state field. Regression fixtures cover HTree conversion,
conversion followed by split, indexed leaf splits with external extents, fragmented linear
link/rename, admission one credit below the bound, 64-credit batching across conversion,
feature/index-flag mismatch, nested admission, remount rejection, stop/abort wakeups and
durable reopen.

### 覆盖写在途 BIO

已映射覆盖写先完整验证映射和数据几何，再在 mount guard 外提交 owned BIO；
原任务持有 inode writeback/invalidate 保护直到全部请求及 metadata 完成，避免
truncate/eviction 释放仍被设备使用的块。最多四个、每个至多 31 页的批次在途；
部分块读改写、分配与 unwritten 不进入该路径。等待与错误清理不持 mount guard。

设备可以乱序失败，完成前缀只跨越连续成功请求；失败后的请求仍须排空。
PageCache completion 在 metadata/额度处理之后于任务上下文消费，Drop 不能先于
设备完成。同步入口不遗留后台错误，原始设备/事务错误优先于收尾错误。现有
ordered-data barrier 和 commit barrier 均保留，BIO 完成不等于持久化完成。
