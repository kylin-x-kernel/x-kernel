# kvfs - 安全与可靠性分析

## 信任模型

用户提供的路径、open flags、rename flags 和 mount flags 不可信。POSIX syscall 层
负责复制用户内存并完成 ABI 初步校验；`kvfs` 接收内核所有的字符串和类型化 flags。
具体文件系统返回的目录项与元数据也必须视为可能失败的外部输入。`kvfs` 在提交
namespace 状态前校验 name、mount relationship、类型、topology 和 operation flags。

`Cred` 来自可信 task 状态，但其 UID/GID 不是“特权保证”，只能作为 DAC 输入。
调用者负责在操作入口取得一个稳定 `Arc<Cred>`；`kvfs` 负责让整次路径遍历、最终检查
和 namespace mutation 使用传入的同一对象。

## 外部边界 / 攻击面

- `Filename::open_with_flags_at` 和 `dentry_open` 是保留 raw `O_*` 的兼容入口，会把
  原始位规范化为 `OpenParams` 与 `OpenFlags`。
- `sys_renameat2` 将 raw rename bits 转换为 `RenameFlags` 后才进入 VFS。
- 文件系统 operation traits 可返回磁盘、网络或设备后端产生的错误与元数据。
- 所有 pathname 与 namespace mutation API 的 `&Cred` 是权限边界；省略或替换它会改变
  当前操作的授权主体。
- `VfsFile::cred()` 暴露 open 时捕获的不可变 credential，供需要 Linux `f_cred` 语义
  的文件操作读取。
- `AnonInodeFs::global()` 是运行时匿名文件创建入口，但它只接受已经由 boot 阶段
  初始化好的 singleton；不从用户输入直接触发全局 VFS 初始化。
- 文件系统 mutation result 会进入 VFS cached inode attributes；错误 identity 或 immutable
  geometry 不可信。

`kvfs` 不直接解引用用户指针，不直接访问 MMIO、PIO、DMA 或 architecture FFI。

## unsafe 代码清单

当前 `fs/kvfs/src` 没有 `unsafe` block。namespace model 由 safe Rust lock、
`Arc` ownership 和 operation trait 的 `Send + Sync` 约束维护内存安全。

## 内存安全不变量

- `LockedDentry` 的 location guard 限定借用的 dentry name 生命周期。
- `parent` 和 `name` 在同一个 location write lock 下同时替换。
- rename 期间 source dentry 和对应 `VfsInode` 保持存活。
- 目录 inode 至多关联一个 live dentry；重复 lookup 必须复用该 alias，不能建立第二套
  child cache 和 topology location。
- create-like callback 成功前必须实例化 VFS 提供的 negative dentry，不能返回另一个
  未经事务校验的对象。
- lookup miss 在 filesystem callback 前发布带 parallel-lookup 状态的 hashed negative
  dentry；同名 lookup 必须等待 owner 完成，不能并行建立第二个 candidate。
- filesystem callback 不能直接修改 VFS cache internals。
- 每个 raw flags 家族必须在边界转换为对应 bitflags 类型。
- 未知 open/rename 位不得进入内部 namespace 或 open 算法。
- `AtomicU32` 中的 `f_flags` 只通过 `OpenFlags` API 读写。
- 不同 flags 类型不得通过 `.bits()` 在 VFS 内互相转换。
- live child dentry 强持有 parent，parent 只保存 child 的弱索引；superblock dcache
  强持有 hashed dentry，驱逐时必须同时移除弱索引和 dcache 所有权。
- `AnonInodeFs` singleton 必须先完成 `init_anon_inodefs()` 发布，后续 `global()`
  返回的引用才有效；运行时路径不得绕过该初始化顺序。
- `Nameidata` 不持有 credential；一次操作的调用者必须在整个方法链中传递同一个 `&Cred`。
- 路径中间目录在 lookup 下一组件前必须通过 `MAY_EXEC`。
- 最终 open 必须按 access mode 检查目标 inode，不能只检查路径是否存在。
- namespace mutation 必须检查相关父目录 `MAY_WRITE | MAY_EXEC`；unlink、rmdir 和 rename
  的 sticky 与 mountpoint policy 必须针对父目录锁内最终 lookup 得到的 victim 执行，不能
  复用锁外预查对象。
- 创建 inode 的初始 UID/GID 必须由 `inode_init_owner()` 基于 `fsuid/fsgid` 和父目录
  setgid 状态导出，不能使用固定 root owner。
- `mkdir` 必须先清除用户 mode 中的 set-user-ID/set-group-ID 位；只有 setgid 父目录可由
  `inode_init_owner()` 给新子目录重新添加 set-group-ID。
- metadata 更新必须先通过 `Path::chown/chmod/set_times` 授权；后端 `setattr` callback
  只执行已授权的 mutation，不能作为 syscall 层的公开绕过入口。
- timestamp 授权必须区分 touch 与显式 times 数组；单个 `UTIME_NOW` 与
  `UTIME_OMIT` 的组合仍属于显式请求。
- descriptor truncate 必须验证 `FMode::WRITE`；pathname truncate 必须使用调用时凭据
  检查 inode write permission。
- dentry backing metadata refresh 必须先确认 positive state，再匹配 inode number、node
  type、block size 和 `rdev`；外部 filesystem bridge 不直接取得内部 `Arc<VfsInode>`。
- split truncate 必须在 inode data lock 下恰好调用一次 `truncate_setsize()`；该入口先发布
  `i_size`，并在 cache truncate 前后各执行一次 mmap invalidation。文件系统负责维护
  backing prepare 后失败的磁盘恢复协议。
- `VfsInode` 只拥有一个 `AddressSpace`，MM/filemap 只经 `VfsFile::mapping()` 获取它；
  不得向 MM 暴露或额外强持有内部 `PageCache`。

## 线程安全

共享 dentry、inode、mount 和 file 状态由 mutex、atomic、`Arc` 和 `Weak` 保护。
可变 per-mount flags 由私有 `AtomicMountFlags` 封装；原子存储只接受和返回强类型
`MountFlags`，并使用 relaxed ordering，因为 flags 不发布或保护其他 mount 状态。
reconfigure 在替换 flags 前校验目标是当前 mount namespace 中已注册 mount 的根路径。
普通 remount 同步更新共享 superblock 的只读策略，bind remount 仅更新目标 mount；
后端固定只读的 superblock 不允许被错误地切换为读写。
children map 与 superblock dcache 不嵌套持锁；namespace 操作先更新 parent 弱索引，
再更新 dcache 强所有权。类型化 flags 是不可变值快照，不提供共享可变状态。

regular-file inode 的 data lock 覆盖 generic buffered write 和整个 set-length callback；
具体文件系统锁在 data lock 内获取。因此同一 inode 的 write、i_size publish、两轮 mmap
invalidation 和 cache truncate 不会交错成两个 EOF source。

所有 namespace lock 都是 sleepable lock。全局顺序为 superblock topology、父目录、
子目录、非目录 inode，最后才是 dentry cache/location。cross-directory 由 parent dentry
identity 决定，父目录锁按 ancestor-first 顺序；互不为祖先时先锁 source parent。目录
inode 依赖单 alias 不变量，非目录 alias 对应的 inode lock 按 identity 去重并按指针值排序。
mount topology lock 始终在相关 inode namespace lock 之后获取。

匿名 inode pseudo fs 的 singleton 由 `Once` 发布，但不允许普通运行时路径触发初始化；
并发创建匿名文件只共享已经发布的 mount/inode，不竞争初始化闭包。

Credential 本身是不可变 `Arc` 快照。权限检查期间不持有 task credential 锁，也不
重新读取 current task，因此并发 `commit_creds()` 只能影响下一次操作。

后端可以在持有 VFS lock 时 sleep。对同一 namespace 对象重新进入 VFS namespace
操作不受支持，可能导致 deadlock。layered filesystem 只能在 upper VFS lock 之后获取
lower filesystem lock；在推广此类嵌套前还需要明确的跨文件系统 lock rank。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 未知 flags 改变控制流 | 中 | 用户传入未支持位 | 边界使用 `from_bits` 并返回 `EINVAL` |
| T-02 | rename 模式冲突 | 中 | `EXCHANGE` 与 `NOREPLACE/WHITEOUT` 组合 | syscall 与 VFS 入口双重校验 |
| T-03 | flags 家族误传 | 中 | 内部 API 使用裸整数 | 独立 bitflags 类型形成编译期隔离 |
| T-04 | dentry 驱逐遗漏导致目录状态或资源生命周期错误 | 中 | namespace 更新只修改弱索引或只修改 dcache | insert/remove/forget 路径成对更新两层缓存，并以行为测试覆盖最后一个外部引用释放后的目录语义 |
| T-05 | 运行时并发首次访问匿名 inode fs 导致初始化卡住 | 中 | 复杂 VFS 对象放在 lazy 首次访问路径中 | boot 阶段调用 `init_anon_inodefs()`，`global()` 只读取已发布对象 |
| T-06 | 路径只检查最终 inode，绕过不可搜索目录 | 高 | namei 未对中间目录检查 execute/search | 每次 lookup 下一组件前调用 `Path::permission(MAY_EXEC, cred)` |
| T-07 | owner class 缺位后错误退回 group/other | 高 | DAC 把三类权限当作可任选集合 | `generic_permission` 按 owner、group、other 互斥顺序只选择一类 |
| T-08 | namespace 修改绕过父目录权限 | 高 | 后端 callback 被直接调用或 VFS wrapper 漏检 | `Path` mutation API 在最终对象仍受 namespace lock 保护时执行父目录 `MAY_WRITE | MAY_EXEC` |
| T-09 | sticky 目录删除其它用户文件 | 高 | 锁外检查的名称在 callback 前被替换 | unlink/rmdir/rename 的 validator 对锁内最终 victim 执行 sticky owner 和 mountpoint policy |
| T-10 | 创建对象固定为 root 或错误组 | 高 | 后端自行填写 UID/GID | 创建 callback 接收 `&Cred` 并使用 `inode_init_owner()` |
| T-11 | 一次 namei 混用 credential | 高 | 每个组件反向调用 current helper | syscall 捕获一次 `Arc<Cred>`，VFS 显式传递引用 |
| T-12 | `ftruncate` 因当前 pathname DAC 被错误拒绝或绕过写模式 | 中 | fd 操作复用 pathname truncate | `VfsFile::truncate` 先验证 `FMode::WRITE`，再走 opened truncate |
| T-13 | 非 owner 直接修改 inode owner、mode 或显式时间 | 高 | syscall 或调用者直接进入后端 `setattr` | `Path` metadata API 在 mount write check 后统一执行 Linux owner/group/write policy |
| T-14 | pathname socket 或其它 special inode 固定为 root | 高 | simple filesystem 动态 mknod 绕过 owner helper | `SimpleDir` mknod 使用 `inode_init_owner()`，仅支持持久插入的目录实现开放创建 |
| T-25 | bind mount 卸载破坏源路径 | 高 | bind root 与源路径共享 dentry，却按独占 mount root 回收 | bind mount 带显式类型标记，卸载只移除 topology，不调用共享 dentry 的 `forget()` |
| T-15 | core mutation result 污染错误的 live inode identity | 高 | bridge 把一个 inode 的 metadata 写入另一个 `VfsInode` | cached metadata refresh 校验 identity、node type、block geometry 和 `rdev` |
| T-16 | truncate 窗口内 private mmap 在新 EOF 后重新 fault | 高 | i_size 晚于 cache truncate 发布，或只执行一次 unmap | inode data lock 串行化 write/truncate；`truncate_setsize()` 执行 i_size publish -> unmap -> cache truncate -> unmap |
| T-17 | unlink 时过早触发磁盘 inode 回收 | 高 | dentry removal 与最后 open-file 引用混为一谈 | `Arc` inode identity 延迟 final teardown，磁盘回收只在 superblock `evict_inode()` hook 中执行 |
| T-18 | 并发 rename 创建目录环 | 高 | cross-directory rename 未序列化 topology 或未在锁内检查祖先关系 | topology mutex、稳定 parent lock 和 ancestry check 拒绝该操作 |
| T-19 | create、link 或 symlink 与 lookup、删除或 replacement 竞争 | 高 | final lookup、对象校验和 mutation 分离持锁 | final lookup、validator、participant lock 和 callback 位于同一父目录 exclusive transaction；create-like callback 复用最终 negative dentry，create-only 错误仅在该对象仍为 negative 时返回 |
| T-26 | 设备节点授权在锁外检查导致错误优先级错误或绕过 | 高 | syscall 先检查特权，或其它 `Path::mknod` 调用者不检查 | `Filename::mknod_at` 与 `Path::mknod` 都在锁内 negative-dentry callback 中进入唯一的 `Path::vfs_mknod`；现有目标先返回 `AlreadyExists` |
| T-27 | open、mkdir 与 mknodat 产生不同 callback mode | 高 | 各入口各自准备 mode/type，或直接调用 inode callback | open、mkdir、regular mknod 和 special mknod 都通过对应的 `Path::vfs_*` 能力执行同一套 DAC、setgid、umask、allowed-permission 与 node-type 策略 |
| T-28 | unlink/rmdir 删除 pathname 初次解析后的同名替代对象 | 高 | syscall 先解析完整目标，再按 parent/name 执行第二次 lookup | `Filename::unlink_at/rmdir_at` 只解析 parent；Dentry 在 parent exclusive lock 下唯一一次解析并操作最终 victim |
| T-20 | 反向 cross-directory rename 死锁 | 高 | 两个线程按相反顺序锁父目录 | 一个 topology mutex 串行化 topology mutation，父目录按拓扑顺序加锁 |
| T-21 | dentry name 和 parent 不一致 | 中 | parent/name 分开更新或读者观察中间状态 | 两个字段在同一个 location write lock 下替换 |
| T-22 | 目录 alias 形成两套 child cache 或绕过 topology 序列化 | 高 | 同一目录 inode 建立多个 live dentry | inode alias 表强制目录单 alias；lookup 复用已有 alias，rename topology 按 parent dentry identity 判定 |
| T-23 | filesystem callback 替换 VFS 已检查的创建对象 | 高 | callback 返回另一个 parent/name 下的 dentry | create-like callback 只能实例化事务 negative dentry；lookup alternate result 校验 positive state、parent 和 name |
| T-24 | 并发 slow lookup 建立重复 dentry | 高 | cache miss 检查与 candidate 发布分离，或等待者提前观察未完成对象 | candidate 在 callback 前原子加入 dcache；owner 保持 parallel-lookup 状态和 lookup mutex，等待者只在 lookup done 后读取结果 |
| T-29 | 卸载先拆 topology 后发现后端写回失败 | 高 | running journal、dirty inode 或 final eviction metadata 尚未持久化 | topology 修改前执行全 superblock sync；普通 mount 在 dentry forget 后再次 sync；任一步失败都保留原 mount tree |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | open 参数非法 | 未知位或非法组合 | open 失败 | 无状态变更 | 3 | 返回 `InvalidInput` |
| F-02 | rename 参数非法 | 模式冲突或 helper 不支持 | rename 失败 | namespace 保持不变 | 2 | 操作前校验 |
| F-03 | 文件系统回调失败 | 后端 I/O 或元数据错误 | 当前操作失败 | 可能降级为 I/O 错误 | 2 | 通过 `VfsResult` 传播 |
| F-04 | hashed dentry 未被及时回收 | 当前阶段没有 Linux shrinker/LRU | dcache 占用增长 | 长期运行可能增加内存压力 | 3 | unlink、rename、forget 显式驱逐；后续接入全局回收策略 |
| F-05 | 匿名 inode fs 未初始化即使用 | boot 初始化顺序缺失 | 当前调用 panic | 暴露启动顺序回归 | 3 | `fs_boot::prepare_namespace()` 显式初始化，测试覆盖启动路径 |
| F-06 | 权限检查失败 | mode、owner 或组不允许请求 | 当前 VFS 操作返回 `PermissionDenied` | namespace 和 inode 状态保持不变 | 3 | 在后端 mutation 前完成通用检查并传播错误 |
| F-07 | 后端不能保存 Unix owner | 磁盘格式没有 UID/GID | getattr 无法完整反映创建身份 | DAC 语义受文件系统能力限制 | 3 | 文档明确后端限制；支持 owner 的后端必须持久化 helper 结果 |
| F-08 | split truncate 在 backing prepare 后 cache invalidation 失败 | 分配或 mapping invalidation 错误 | 当前 truncate 返回失败 | backing inode 可能保持 orphan/recovery state | 2 | 由具体文件系统的持久化 recovery protocol 收敛，禁止静默执行 finish |
| F-09 | unmount writeback 失败 | 设备或 filesystem sync 错误 | 当前 unmount 返回失败 | mount 继续可见且可重试，不会静默丢弃 dirty state | 2 | dentry forget 和 topology detach 前同步；普通 mount 的 forget 后再同步一次 |

校验失败会在 filesystem callback 前返回 typed VFS error。后端失败时 dentry cache
location 保持不变，因为 cache commit 只在 callback 成功后执行；rename 所需 key 和 cache
slot 在 callback 前已经存在，commit 只交换 location 和原位替换 slot，不执行可能失败的
分配。持久化操作的 rollback 与 logging 仍由后端负责。

## 已知限制

- 尚无 capability、LSM、POSIX ACL、user namespace ID 映射和 idmapped mount DAC。
- FAT 等后端不能完整表达 Unix UID/GID owner。
- 当前 POSIX rename 路径不支持 `RENAME_WHITEOUT`。inode lookup 和 getattr 的类型已
  建立，但尚未定义额外语义位；当前调用使用 empty flags。
- superblock dentry cache 尚未实现 Linux 风格的 LRU/shrinker，当前依赖 namespace
  删除和卸载路径主动驱逐。
- KVFS 提供 AddressSpace view invalidation 通知，但各文件系统仍需用 live mmap/truncate
  case 验证自身接线。
- fast lookup 仍是 mutex-based，没有 RCU 或 rename sequence validation；同名 slow lookup
  通过 hashed candidate 的 owner/waiter 协议合并。
- layered filesystem 的 lock ordering 尚未建模。
- mount topology synchronization 与 superblock rename mutex 是不同机制，但均遵循已记录的
  inode-then-topology 嵌套顺序。

## 审计清单

- 新 syscall flags 是否在 ABI 边界完成类型转换。
- 内部代码是否使用 `contains`/`intersects`，而不是重新按位解析整数。
- `.bits()` 是否只用于 ABI 输出、原子存储或明确的底层接口。
- 新 flags 组合是否补充冲突校验与行为测试。
- dentry cache 插入、rename、unlink、forget 是否保持 parent 弱索引与 superblock
  强所有权同步。
- 没有外部 `Dentry` 引用时，hashed child 是否仍能参与目录非空判断。
- 新增匿名 inode 使用点是否只调用 `AnonInodeFs::global()`，且不重新引入 lazy 首次访问
  初始化。
- 新 pathname 入口是否显式接收并逐层传递同一个 `&Cred`。
- 中间目录 search、最终 inode 和父目录 mutation 权限是否分别在正确阶段检查。
- 新建 inode 是否使用 `inode_init_owner()`，后端是否持久化 UID/GID。
- fd-based 操作是否使用 open file mode/`f_cred`，而不是重新执行 pathname 授权。
- backing mutation 后是否刷新所有受影响的 live inode，而不是只返回新 dentry。
- truncate 是否通过 `set_len()` 与 `truncate_setsize()` 保持 backing prepare、i_size publish、
  两轮 AddressSpace invalidation、cache truncate 和 backing finish 顺序。
- file-backed MM runtime 是否只保存 `VfsFile`，并经 `VfsFile::mapping()` 访问唯一的
  inode `AddressSpace`。
- `release()` 与 final `evict_inode()` 是否保持为两个不同生命周期阶段。
- 每个 namespace mutation 是否获取父目录 exclusive lock。
- unlink/rmdir 和 rename replacement 是否锁住 victim inode。
- cross-directory rename 是否先获取 topology mutex，再获取 inode lock。
- directory lock 是否先于 non-directory lock 获取。
- callback 是否避免重新进入同一组 VFS namespace 对象。
- cache commit 是否只在 backend success 后执行。
