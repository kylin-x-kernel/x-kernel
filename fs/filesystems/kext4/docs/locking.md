# KExt4 - 并发与锁顺序

## 目标

本文定义 KExt4 的锁域、锁顺序和阻塞规则。所有实现必须遵守本文，避免重新出现
旧 `rsext4` 使用单个 `Mutex<Ext4State>` 串行整个文件系统的问题。

锁的目标是保护不变量，不是保护整个调用路径。不得因为暂时方便而新增覆盖多个
所有权域的全局大锁。

## 基本规则

1. 每个共享可变状态必须在类型定义附近注明保护它的锁。
2. 同时获取多个同类对象锁时，使用稳定 ID 升序。
3. 不持有 spinlock、IRQ-safe lock 或底层 journal 内部锁等待磁盘 I/O。
4. 不持有 metadata buffer 内容锁调用可能重新读取同一 buffer 的代码。
5. 不持有 allocator group lock 修改 extent、目录或 inode。
6. 不持有 PageCache 页面锁等待 journal commit。
7. transaction handle 是事务引用和 credits，不是替代业务锁的互斥锁。
8. 任何违反标准顺序的 try-lock/retry 路径必须在代码和本文中说明。

## 锁域与所有者

| 锁域 | 保护状态 | 所有者 |
|---|---|---|
| mount lifecycle | freeze、unmount、read-only、abort | A |
| inode namespace | link count、目录关系、rename 语义 | B |
| inode metadata | size、mode、timestamps、extent root | B |
| inode data mapping | extent tree、delalloc/unwritten 转换 | B |
| KFS mapping/page | cached page、dirty/writeback、LRU | KFS/B |
| block group | bitmap、buddy、group free counters | A |
| metadata buffer | 块内容、dirty、I/O state | A |
| journal transaction | running/committing/checkpoint 状态 | A |
| block device queue | driver I/O 队列 | 驱动 |

一个锁不能同时声称保护两个目录所有者的全部状态。跨域原子性由 transaction、
状态机和规定的锁顺序共同实现。

## 逻辑锁顺序

从外到内的默认顺序：

```text
L0  mount lifecycle / freeze read guard
L1  directory and inode namespace locks
L2  inode metadata/data mapping lock
L3  KFS mapping index lock
L4  cached page content/state lock
L5  block-group allocator lock
L6  metadata-buffer content lock
L7  journal internal lock
L8  block-device internal lock
```

说明：

- transaction handle 不占一个固定层级；它必须在首次修改 metadata buffer
  之前创建，并在进入 L6/L7 前预留足够 credits。
- allocator 可以在 L2 下工作，但进入 L5 后不能回调 extent 或 inode 代码。
- metadata buffer 的 `write(tx)` guard 可以短暂进入 L6/L7，但 guard 生命周期内
  不能获取 L1-L5 的新锁。
- KFS 的具体内部锁顺序由 KFS 定义；KExt4 回调 PageCache 时不得持有 L5-L7。

## 同层多对象顺序

### Inode 和目录

同时锁多个 inode 时按以下键升序：

```text
(filesystem instance id, inode number)
```

特殊操作：

- rename 同时涉及两个父目录时，先锁 inode number 较小的父目录；
- 再按 inode number 锁源 inode 和目标 inode；
- 同一 inode 只锁一次；
- `.`、`..` 和目录祖先检查不能在持有无序子目录锁时递归。

若 Linux 语义要求目录层级顺序优先于 inode number，应实现专用 rename
协调器，并将其视为 L1 的唯一入口，不能在各调用点自行发明顺序。

### Block group

同时操作多个 block group 时按 group number 升序。优先把请求拆成单 group
操作，避免长期持有多个 group lock。

### Metadata buffer

必须同时锁多个 metadata buffer 时按 physical block number 升序。extent
split 等无法提前知道全部块号的操作应分阶段执行，或者使用 try-lock 后释放并
按排序结果重试。

## Transaction 使用规则

### 创建

- 在首次请求 metadata write access 前调用 `Journal::begin`；
- credits 根据操作最坏修改块数估算；
- `begin` 可能阻塞等待 journal 空间，调用时不能持有 L4-L8；
- 允许在 L1/L2 下 begin，以支持依赖锁内验证结果的 namespace/extent 操作；
- 能在锁外可靠估算和建立 handle 的路径应优先锁外 begin，缩短业务锁持有时间；
- 同一种操作必须只有一种固定顺序，不能有时先 begin、有时先获取 L1/L2。

### 使用

- handle 只能在当前执行路径使用，默认不跨线程；
- `MetadataBuffer::write` 必须接收 handle；
- credits 不足时释放 L5/L6，再扩展或重启 transaction；
- transaction abort 后，所有后续修改立即失败。

### 完成和提交

- handle drop 表示当前操作结束，不表示 transaction 已持久化；
- 普通写入允许 transaction 批量 commit；
- fsync 可以请求并等待指定 transaction commit；
- 等待 commit/checkpoint 时不得持有 L1-L7；
- ordered data completion 的等待发生在 journal commit worker 中，但不能
  持有 journal spinlock。

## I/O 等待规则

允许等待 I/O 的位置：

- PageCache 缺页加载，在不持有 inode namespace、group、buffer 内容锁时；
- metadata buffer 首次读取，在未持有同一 buffer 锁时；
- writeback completion，在不持有页面内容锁时；
- fsync 最终等待，在释放 inode/extent 修改锁后；
- journal worker 等待 ordered data，不持有内部短锁。

禁止等待 I/O 的位置：

- block group bitmap 锁内；
- metadata buffer write guard 生命周期内；
- inode rename 多对象锁全部持有时；
- PageCache eviction listener 回调持有地址空间页表锁时；
- journal state spinlock 内。

## 关键操作锁序

### Buffered read miss

```text
获取 inode data read lock (L2)
查询 extent mapping
释放 inode data read lock
提交 data read I/O
完成后发布 PageCache page
```

不能在等待数据 I/O 时长期持有 inode data write lock。

### Delayed-allocation writeback

```text
建立 transaction handle
锁 inode data mapping (L2)
确认 delalloc range
调用 allocator:
  短暂锁 group (L5)
  返回 physical extent
修改 extent metadata:
  按块号锁 metadata buffer (L6)
释放 inode data mapping
提交数据 I/O
登记 ordered completion
结束 handle
```

如果 extent 更新需要更多 metadata block，先退出 L5/L6，再申请扩展 credits
或分配块，禁止锁顺序回退。

### Truncate

```text
阻止目标范围的新 PageCache 写入
锁 inode metadata/data mapping (L2)
建立或确认 transaction credits
从 extent tree 移除映射
把待释放物理 extent 记录到局部列表
更新 inode size
释放 inode data mapping
失效 EOF 后 PageCache pages
按 group 顺序释放物理块 (L5)
结束 transaction
```

实现必须明确 crash 中间态如何通过 orphan/journal 恢复，不能只依赖内存列表。

### Rename

```text
获取 mount read/freeze guard (L0)
按规则锁父目录和相关 inode (L1)
建立 transaction
读取并修改目录 metadata buffers (L6，短临界区)
更新 link count、ctime 和 orphan 状态
释放 buffer 锁
释放 inode/目录锁
结束 transaction
```

不得在持有目录锁时等待 transaction commit。

### Fsync

```text
锁 inode 以截取目标 dirty range/transaction id
释放 inode 锁
触发并等待 PageCache writeback
等待 ordered data completion
等待相关 journal transaction commit
必要时 flush device
```

## PageCache 回调约束

KExt4 的 `read_pages/write_pages` 回调可能睡眠和执行 I/O。

- KFS 不应在调用它们时持有全局 LRU 锁；
- KExt4 不应在回调 KFS 页面完成函数时持有 group/buffer/journal 内部锁；
- writeback 完成通过 completion/state transition 通知；
- eviction listener 只负责撤销映射，不做 journal commit；
- direct I/O 必须先执行 PageCache range flush/invalidate 协议。

## 内存分配约束

- 持有 L5-L7 时避免可能阻塞或触发文件系统回收的内存分配；
- transaction 开始前预分配操作所需的小型描述符；
- writeback 批次使用有上限的向量或预分配池；
- reclaim 路径不能递归进入同一文件系统的普通分配路径；
- 不在热路径创建按页 heap object，优先复用 PageCache/metadata buffer 状态。

## Review 检查表

每个涉及共享状态的 PR 必须回答：

- 新字段由哪把锁保护？
- 是否新增了两把锁同时持有的路径？
- 多 inode/group/buffer 的排序键是什么？
- 哪些调用可能睡眠或等待 I/O？
- 是否在锁内调用了对方模块？
- credits 不足、I/O 失败、transaction abort 时如何退出？
- fsync/commit 等待前是否释放业务锁？
- 是否存在 PageCache -> KExt4 -> PageCache 的锁循环？
- 是否用全局锁掩盖了尚未设计的不变量？

## 调试要求

开发阶段建议提供：

- 可选 lock-rank 断言；
- inode/group/buffer 锁等待时间统计；
- journal transaction 等待和 credits 扩展计数；
- PageCache writeback 批次大小统计；
- 超长临界区告警。

性能优化 PR 不能通过删除锁保护或放宽一致性语义来降低指标。
