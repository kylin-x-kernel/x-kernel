# kvfs - 安全与可靠性分析

## 信任模型

用户提供的路径、open flags、rename flags 和 mount flags 不可信。POSIX syscall 层
负责复制用户内存并完成 ABI 初步校验；`kvfs` 接收内核所有的字符串和类型化 flags。
具体文件系统返回的目录项与元数据也必须视为可能失败的外部输入。

`Cred` 来自可信 task 状态，但其 UID/GID 不是“特权保证”，只能作为 DAC 输入。
调用者负责在操作入口取得一个稳定 `Arc<Cred>`；`kvfs` 负责让整次路径遍历、最终检查
和 namespace mutation 使用传入的同一对象。

## 外部边界 / 攻击面

- `Filename::open_with_flags_at` 和 `dentry_open` 是保留 raw `O_*` 的兼容入口。
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

`kvfs` 不直接解引用用户指针，不直接访问 MMIO、PIO 或 DMA。

## unsafe 代码清单

当前 `fs/kvfs/src` 没有 `unsafe` block。内存安全依赖 Rust 所有权以及 operation
trait 的 `Send + Sync` 约束。

## 内存安全不变量

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
- namespace mutation 必须先检查相关父目录 `MAY_WRITE | MAY_EXEC`；sticky 目录必须额外
  检查目录所有者、victim 所有者或 root。
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
- split truncate 成功时必须恰好执行一次 PageCache resize；文件系统负责维护 prepare 后失败
  的磁盘恢复协议。

## 线程安全

共享 dentry、inode、mount 和 file 状态由 mutex、atomic、`Arc` 和 `Weak` 保护。
children map 与 superblock dcache 不嵌套持锁；namespace 操作先更新 parent 弱索引，
再更新 dcache 强所有权。类型化 flags 是不可变值快照，不提供共享可变状态。

匿名 inode pseudo fs 的 singleton 由 `Once` 发布，但不允许普通运行时路径触发初始化；
并发创建匿名文件只共享已经发布的 mount/inode，不竞争初始化闭包。

Credential 本身是不可变 `Arc` 快照。权限检查期间不持有 task credential 锁，也不
重新读取 current task，因此并发 `commit_creds()` 只能影响下一次操作。

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
| T-08 | namespace 修改绕过父目录权限 | 高 | 后端 callback 被直接调用或 VFS wrapper 漏检 | `Path` mutation API 统一执行父目录 `MAY_WRITE | MAY_EXEC` |
| T-09 | sticky 目录删除其它用户文件 | 高 | unlink/rename 只检查目录 mode | VFS 在回调前执行 sticky owner policy |
| T-10 | 创建对象固定为 root 或错误组 | 高 | 后端自行填写 UID/GID | 创建 callback 接收 `&Cred` 并使用 `inode_init_owner()` |
| T-11 | 一次 namei 混用 credential | 高 | 每个组件反向调用 current helper | syscall 捕获一次 `Arc<Cred>`，VFS 显式传递引用 |
| T-12 | `ftruncate` 因当前 pathname DAC 被错误拒绝或绕过写模式 | 中 | fd 操作复用 pathname truncate | `VfsFile::truncate` 先验证 `FMode::WRITE`，再走 opened truncate |
| T-13 | 非 owner 直接修改 inode owner、mode 或显式时间 | 高 | syscall 或调用者直接进入后端 `setattr` | `Path` metadata API 在 mount write check 后统一执行 Linux owner/group/write policy |
| T-14 | pathname socket 或其它 special inode 固定为 root | 高 | simple filesystem 动态 mknod 绕过 owner helper | `SimpleDir` mknod 使用 `inode_init_owner()`，仅支持持久插入的目录实现开放创建 |
| T-06 | core mutation result 污染错误的 live inode identity | 高 | bridge 把一个 inode 的 metadata 写入另一个 `VfsInode` | cached metadata refresh 校验 identity、node type、block geometry 和 `rdev` |
| T-07 | truncate 先释放磁盘 block、后失效 PageCache/mmap，造成 stale access | 高 | 文件系统没有保持 split truncate 顺序 | 文件系统 `set_len()` 执行 backing prepare → `truncate_pagecache()`/view invalidation → backing finish |
| T-08 | unlink 时过早触发磁盘 inode 回收 | 高 | dentry removal 与最后 open-file 引用混为一谈 | `Arc` inode identity 延迟 final teardown，磁盘回收只在 superblock `evict_inode()` hook 中执行 |

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
| F-06 | split truncate 在 backing prepare 后 cache invalidation 失败 | 分配或 mapping invalidation 错误 | 当前 truncate 返回失败 | backing inode 可能保持 orphan/recovery state | 2 | 由具体文件系统的持久化 recovery protocol 收敛，禁止静默执行 finish |

## 已知限制

- 尚无 capability、LSM、POSIX ACL、user namespace ID 映射和 idmapped mount DAC。
- FAT 等后端不能完整表达 Unix UID/GID owner。
- 当前 POSIX rename 路径不支持 `RENAME_WHITEOUT`。inode lookup 和 getattr 的类型已
  建立，但尚未定义额外语义位；当前调用使用 empty flags。
- superblock dentry cache 尚未实现 Linux 风格的 LRU/shrinker，当前依赖 namespace
  删除和卸载路径主动驱逐。
KVFS 提供 Mapping view
invalidation 通知，但各文件系统仍需用 live mmap/truncate case 验证自身接线。

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
- backing mutation 后是否刷新所有受影响的 live inode，而不是只返回新 dentry？
- truncate 是否通过 `set_len()` 与 `truncate_pagecache()` 保持 backing prepare、Mapping invalidation 和
  backing finish 顺序？
- `release()` 与 final `evict_inode()` 是否保持为两个不同生命周期阶段？
