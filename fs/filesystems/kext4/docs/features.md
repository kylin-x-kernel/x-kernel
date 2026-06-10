# KExt4 - 功能范围与性能验收

## 目标定义

KExt4 的“完整”含义是：

- 覆盖 X-Kernel 目标工作负载需要的 ext4 核心功能；
- 与 Linux ext4 创建的受支持镜像双向兼容；
- 提供可靠 journal、恢复、fsync、mmap 和并发语义；
- 对明确不支持的 feature 安全拒绝或只读挂载；
- 在固定基准上优于旧 `rsext4`。

它不要求实现 Linux ext4 的每个历史、企业或硬件专用特性。

## 状态定义

| 状态 | 含义 |
|---|---|
| Required | 发布目标必须支持并测试 |
| Planned | 架构必须允许，核心稳定后实现 |
| Deferred | 当前不实现，遇到时明确降级或拒绝 |
| Rejected | 不计划实现，必须安全拒绝 |

每项功能还应在实现后标记：

- `read-only`
- `read-write`
- `recovery-tested`
- `performance-tested`

## Required 功能

### 磁盘与挂载

| 功能 | 负责人 | 验收要求 |
|---|---|---|
| superblock/group descriptor | A | Linux 镜像挂载、范围和 checksum 校验 |
| variable block size | A | 至少 1K/2K/4K，按平台约束测试 |
| 64-bit block number | A | 大于 32-bit 边界的格式和运算测试 |
| metadata checksum | A | 正常、损坏、错误定位测试 |
| clean/dirty mount state | A | 非正常卸载后恢复 |
| read-only/error policy | A | I/O 错误后符合挂载策略 |
| unmount/flush/barrier | A | 无脏 metadata 和未提交 transaction |

### inode 与文件

| 功能 | 负责人 | 验收要求 |
|---|---|---|
| regular file | B | buffered read/write、sparse、EOF |
| extents | B | lookup/insert/split/merge/truncate |
| inode cache | B | 同 inode 唯一内存身份 |
| hard link | B | 多 dentry 共享 inode/PageCache |
| symlink | B | fast 和 block-backed symlink |
| special inode | B | VFS 类型和 rdev 正确 |
| truncate | B+A | crash recovery 和 PageCache 一致性 |
| fallocate basic modes | B+A | allocate、keep-size、punch-hole |
| mmap shared/private | B | buffered I/O 可见性和回写 |
| fsync/fdatasync | B+A | 数据、journal、flush 顺序测试 |

### 目录与命名空间

| 功能 | 负责人 | 验收要求 |
|---|---|---|
| linear directory | B | lookup/readdir/create/remove |
| HTree indexed directory | B | 大目录和 hash collision |
| mkdir/rmdir | B+A | link count 和恢复 |
| link/unlink | B+A | orphan 和最后引用释放 |
| rename | B+A | 跨目录、覆盖、崩溃原子性 |
| stable readdir behavior | B | 合理处理并发修改 |

### 空间管理

| 功能 | 负责人 | 验收要求 |
|---|---|---|
| inode bitmap | A | `u64` 扫描、hint、checksum |
| block bitmap | A | `u64` 扫描、hint、checksum |
| multiblock allocator | A | 连续分配优先、短分配处理 |
| buddy/free extent cache | A | 与 bitmap 一致性验证 |
| locality/preallocation | A | 顺序写碎片率和吞吐 |
| ENOSPC recovery | A+B | delalloc 回退和错误返回 |

### Journal 与恢复

| 功能 | 负责人 | 验收要求 |
|---|---|---|
| JBD2 descriptor/commit/revoke | A | 与 Linux journal 格式兼容 |
| `data=ordered` | A+B | metadata commit 前数据完成 |
| transaction credits | A | 不足时扩展/重启，无越界修改 |
| checkpoint | A | journal 空间回收和错误处理 |
| journal replay | A | 多个断电点恢复 |
| orphan recovery | A+B | unlink/truncate 崩溃恢复 |
| journal abort | A | I/O 错误后停止危险写入 |

### 属性

| 功能 | 负责人 | 验收要求 |
|---|---|---|
| uid/gid/mode/timestamps | B | Linux 可观察语义 |
| user/security xattr 基础 | B | inline/external xattr block |
| POSIX ACL | B | 权限检查和继承 |
| statfs | A+B | free count 与 allocator 一致 |

## Planned 功能

这些功能不阻塞首个完整稳定版本，但架构不得封死：

- project quota；
- large directory 增强；
- orphan file；
- fast commit；
- online resize；
- extent status tree 的高级优化；
- async discard；
- richer ioctl；
- lazy inode-table initialization；
- flex_bg 优化；
- big-endian 主机验证；
- direct I/O 高级并发优化。

## Deferred 功能

除非目标工作负载改变，以下功能延后：

- `data=journal`；
- `data=writeback` 的完整兼容；
- fscrypt；
- fsverity；
- casefold；
- inline data；
- bigalloc；
- DAX；
- MMP；
- snapshot 类外部扩展；
- 老式 indirect block 的读写支持。

如果只提供只读兼容，必须在 feature 矩阵和 mount 日志中明确。

## Rejected 或必须拒绝的情况

- 未识别的 `INCOMPAT` feature；
- 超出设备容量的 block/inode/descriptor；
- 不受支持且无法安全忽略的 checksum 类型；
- journal 格式或 block size 无法处理；
- on-disk 结构大小、对齐或范围验证失败；
- 需要未实现加密密钥语义的读写挂载。

不得为了“尽量挂载”而忽略上述条件。

## Feature bit 策略

实现时在代码中维护与本文同步的表：

```text
feature bit
  -> supported read-only?
  -> supported read-write?
  -> requires journal?
  -> recovery tested?
  -> Linux interoperability tested?
```

挂载输出应包含：

- 拒绝或降级原因；
- 未支持 feature 名称和 bit；
- 是否进入只读模式；
- journal 是否完成 recovery。

## 性能目标

### 基线

每次里程碑固定对比：

1. 旧 `rsext4`；
2. KExt4；
3. 同类环境中的 Linux ext4，作为参考上限而非硬性等同比较。

镜像大小、块大小、设备模型、CPU 数、内存、编译模式和 fio 配置必须固定并
记录。

### fio 矩阵

至少覆盖：

| 维度 | 取值 |
|---|---|
| 模式 | seq read/write、rand read/write、混合 |
| block size | 4K、64K、1M |
| jobs | 1、4、CPU 数 |
| iodepth | 1、8、32（受当前 I/O 栈能力限制时注明） |
| 路径 | buffered、mmap、direct I/O（支持后） |
| durability | 无 fsync、周期 fsync、每次 fsync |
| cache | cold、warm |
| 文件 | 单大文件、多文件 |

### 元数据基准

- 小文件批量 create/unlink；
- 多线程 create；
- 同目录和多目录 rename；
- hard-link churn；
- 大目录 lookup/readdir；
- fsync 新文件和父目录；
- mount/recovery 时间。

### 资源与并发指标

除吞吐和延迟外，记录：

- 实际 block I/O 次数和平均 request size；
- PageCache hit/miss 和 writeback batch size；
- extent 数量和碎片率；
- journal commit 次数、批次和等待时间；
- block group/inode/page 锁竞争；
- metadata buffer 命中率；
- 内存占用；
- CPU cycles 或可用的热点采样。

### 验收阈值

稳定版本的最低要求：

- Required 正确性测试全部通过；
- 不以关闭 journal、checksum 或 flush 换取性能；
- 主要 fio 矩阵的几何平均吞吐高于 `rsext4`；
- 主要元数据基准不出现系统性退化；
- 4K 多线程负载不被单一全局锁串行；
- 顺序写能形成多块 I/O，而不是持续每页一次设备提交；
- 回归超过约定阈值时阻止合并并附分析。

具体百分比应在首个可写版本建立基线后写入 CI 配置，避免现在凭空设定。

## 正确性测试

### 格式互操作

- Linux `mkfs.ext4` 创建，KExt4 挂载和修改，Linux `fsck.ext4` 验证；
- KExt4 修改后的镜像由 Linux ext4 挂载验证；
- 不同 block/inode size 和 feature 组合；
- metadata checksum 损坏注入。

### 崩溃注入

在以下边界模拟掉电：

- 数据写前/后；
- extent 更新前/后；
- journal descriptor、data、commit block 之间；
- checkpoint 前/后；
- rename 各关键 metadata 更新之间；
- truncate/orphan 添加与删除之间；
- fsync 返回前的每个持久化阶段。

恢复后必须检查：

- 文件系统可挂载或明确报告不可恢复错误；
- 不暴露旧磁盘数据；
- bitmap、free count、extent 和 inode 一致；
- rename/link count 满足允许的原子结果；
- 已成功返回的 fsync 数据存在。

### 并发测试

- 同 inode 多线程读写和 truncate；
- mmap 与 write/truncate；
- 多 inode 同 group 分配；
- rename 与 lookup/readdir；
- fsync 与后台 writeback；
- ENOSPC 与并发分配；
- journal abort 与正在进行的操作。

## 里程碑规则

里程碑可以分阶段交付，但每一阶段都必须沿用最终架构：

- 不允许为了临时可运行引入全局 `Ext4State` 锁；
- 不允许临时加入第二套文件数据 cache；
- 不允许无 journal 的写路径在后续难以接入 transaction；
- 不允许用手写字段复制形成永久 on-disk API；
- 不允许把 delayed allocation、ordered writeback 的状态塞进无类型布尔值。

阶段性缺失功能通过明确的 `UnsupportedFeature` 或只读模式表达，而不是构建
未来必须推倒的旁路实现。

## 功能变更流程

新增或改变 feature 前：

1. 更新本文件的状态、负责人和验收要求；
2. 检查 `api.md` 是否需要新跨层契约；
3. 检查 `locking.md` 是否引入新锁或等待路径；
4. 检查 `vfs.md` 是否需要新的 inode、PageCache 或一致性能力；
5. 指定 Linux 互操作和 crash 测试；
6. 指定性能基准；
7. 由双方 review 后开始实现。
