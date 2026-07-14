# kvfs - 安全与可靠性分析

## 信任模型

用户提供的路径、open flags、rename flags 和 mount flags 不可信。POSIX syscall 层
负责复制用户内存并完成 ABI 初步校验；`kvfs` 接收内核所有的字符串和类型化 flags。
具体文件系统返回的目录项与元数据也必须视为可能失败的外部输入。

## 外部边界 / 攻击面

- `Filename::open_with_flags_at` 和 `dentry_open` 是保留 raw `O_*` 的兼容入口。
- `sys_renameat2` 将 raw rename bits 转换为 `RenameFlags` 后才进入 VFS。
- 文件系统 operation traits 可返回磁盘、网络或设备后端产生的错误与元数据。

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

## 线程安全

共享 dentry、inode、mount 和 file 状态由 mutex、atomic、`Arc` 和 `Weak` 保护。
children map 与 superblock dcache 不嵌套持锁；namespace 操作先更新 parent 弱索引，
再更新 dcache 强所有权。类型化 flags 是不可变值快照，不提供共享可变状态。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 未知 flags 改变控制流 | 中 | 用户传入未支持位 | 边界使用 `from_bits` 并返回 `EINVAL` |
| T-02 | rename 模式冲突 | 中 | `EXCHANGE` 与 `NOREPLACE/WHITEOUT` 组合 | syscall 与 VFS 入口双重校验 |
| T-03 | flags 家族误传 | 中 | 内部 API 使用裸整数 | 独立 bitflags 类型形成编译期隔离 |
| T-04 | dentry 驱逐遗漏导致目录状态或资源生命周期错误 | 中 | namespace 更新只修改弱索引或只修改 dcache | insert/remove/forget 路径成对更新两层缓存，并以行为测试覆盖最后一个外部引用释放后的目录语义 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | open 参数非法 | 未知位或非法组合 | open 失败 | 无状态变更 | 3 | 返回 `InvalidInput` |
| F-02 | rename 参数非法 | 模式冲突或 helper 不支持 | rename 失败 | namespace 保持不变 | 2 | 操作前校验 |
| F-03 | 文件系统回调失败 | 后端 I/O 或元数据错误 | 当前操作失败 | 可能降级为 I/O 错误 | 2 | 通过 `VfsResult` 传播 |
| F-04 | hashed dentry 未被及时回收 | 当前阶段没有 Linux shrinker/LRU | dcache 占用增长 | 长期运行可能增加内存压力 | 3 | unlink、rename、forget 显式驱逐；后续接入全局回收策略 |

## 已知限制

当前 POSIX rename 路径不支持 `RENAME_WHITEOUT`。inode lookup 和 getattr 的类型已
建立，但尚未定义额外语义位；当前调用使用 empty flags。superblock dentry cache
尚未实现 Linux 风格的 LRU/shrinker，当前依赖 namespace 删除和卸载路径主动驱逐。

## 审计清单

- 新 syscall flags 是否在 ABI 边界完成类型转换。
- 内部代码是否使用 `contains`/`intersects`，而不是重新按位解析整数。
- `.bits()` 是否只用于 ABI 输出、原子存储或明确的底层接口。
- 新 flags 组合是否补充冲突校验与行为测试。
- dentry cache 插入、rename、unlink、forget 是否保持 parent 弱索引与 superblock
  强所有权同步。
- 没有外部 `Dentry` 引用时，hashed child 是否仍能参与目录非空判断。
