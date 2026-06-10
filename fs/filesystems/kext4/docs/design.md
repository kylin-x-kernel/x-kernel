# KExt4 - 设计与协作边界

## 定位

KExt4 是 X-Kernel 新的 ext4 实现。它以 Linux ext4 的对象边界、磁盘格式、
事务模型和关键数据路径为主要参考，但不以逐行复刻 Linux 为目标。

项目目标是：

- 兼容选定的 Linux ext4 磁盘格式和常用语义；
- 保持元数据一致性和可恢复性；
- 避免旧 `rsext4` 的全局大锁、重复数据缓存和细粒度同步 I/O；
- 在约定的 fio 和元数据工作负载中稳定优于 `rsext4`；
- 允许两名开发者长期并行开发，而不重复拥有同一份状态。

本文是模块所有权和协作规则的主文档。接口、锁和功能范围分别见：

- `api.md`
- `locking.md`
- `features.md`
- `vfs.md`

## 核心原则

### 一个状态只有一个所有者

同一种持久化状态或缓存不能在两个子系统中各维护一份权威副本。

| 状态 | 唯一所有者 |
|---|---|
| 文件数据页、dirty/writeback 状态 | KFS `FileMapping` / PageCache |
| 内存 inode、文件大小、extent 状态协调 | 文件语义层 |
| extent tree 的解析和修改算法 | 文件语义层 |
| 元数据块缓存及 buffer 生命周期 | 存储一致性层 |
| block/inode bitmap | 存储一致性层 |
| block group 空闲空间状态 | 存储一致性层 |
| journal transaction 和 checkpoint | 存储一致性层 |
| on-disk 结构、endian、checksum | 存储一致性层 |

extent tree 节点位于元数据 buffer 中，但：

- buffer 的读取、锁定、dirty、回写和 journal 归存储一致性层；
- extent 节点的格式解释、查找、分裂和合并归文件语义层。

### 单向依赖

```text
kvfs / KFS PageCache
          |
          v
vfs + inode + file + directory + extent + writeback       成员 B
          |
          v
                    api
          |
          v
superblock + allocator + metadata buffer + journal + I/O   成员 A
          |
          v
                BlockDevice
```

允许上层调用下层，禁止下层依赖具体 VFS、目录、extent 或 PageCache 类型。

### 文件数据不重复缓存

普通文件数据的长期缓存只有 KFS PageCache。KExt4 不实现类似
`datablock_cache` 的第二套文件数据缓存。

元数据需要独立的 metadata buffer cache，因为它必须参与 checksum、
journal、checkpoint 和恢复流程。metadata buffer 不能冒充文件 PageCache，
文件 PageCache 也不能绕过 journal 直接修改元数据块。

## 长期人员分工

文档使用“成员 A”和“成员 B”表示角色，实际人员可以调整，但一个迭代内不能
同时交换目录所有权。

### 成员 A：存储一致性层

成员 A 对“磁盘上最终是什么状态、断电后如何恢复”负责。

独占负责：

- `disk/`：on-disk POD、endian、feature bit、checksum；
- `io/`：块设备适配、批量 I/O、flush/barrier；
- `buffer/`：metadata buffer cache、dirty 和 I/O 状态；
- `superblock/`：挂载校验、group descriptor、错误策略；
- `alloc/`：block/inode bitmap、buddy、多块分配、预分配；
- `journal/`：JBD2 transaction、commit、checkpoint、recovery；
- journal replay、orphan recovery 的底层机制；
- 存储错误后的 abort、只读降级和持久化保证。

成员 A 不得：

- 实现第二套文件数据缓存；
- 解释 VFS 路径或目录语义；
- 直接修改内存 inode 的业务状态；
- 从 journal 或 allocator 反向调用 VFS 操作。

### 成员 B：文件语义与数据路径

成员 B 对“VFS 调用后用户应当观察到什么”负责。

独占负责：

- `vfs/`：`FilesystemOps`、`NodeOps`、`FileNodeOps`、`DirNodeOps` 适配；
- `inode/`：内存 inode、inode cache、生命周期和 inode 级同步；
- `extent/`：逻辑块映射、extent tree、delayed allocation 映射；
- `dir/`：目录项、HTree、lookup/readdir/create/unlink/rename/link；
- `file/`：truncate、fallocate、fsync、symlink、特殊文件；
- `writeback/`：PageCache 接入、聚合写回、ordered-data 完成事件；
- `xattr/`：xattr、POSIX ACL 和相关 inode 语义；
- mmap、direct I/O 与 buffered I/O 的一致性。

成员 B 不得：

- 直接扫描或修改 block/inode bitmap；
- 绕过 metadata buffer 修改裸元数据块；
- 自行提交 journal descriptor/commit block；
- 直接持有块设备并绕过存储一致性层写元数据。

### 共同负责的交叉路径

以下功能由一人担任当次功能负责人，另一人必须 review：

- mount/unmount；
- buffered writeback 和 delayed allocation；
- truncate、fallocate、punch hole；
- create、unlink、rename、orphan；
- fsync 和 fdatasync；
- journal recovery；
- direct I/O coherence；
- 锁顺序或公共 API 变更。

功能负责人负责集成测试和最终 PR，但不能在同一个 PR 中顺手重构另一人的
内部模块。

## 目标目录

```text
kext4/
├── Cargo.toml
├── docs/
│   ├── design.md
│   ├── api.md
│   ├── locking.md
│   ├── features.md
│   └── vfs.md
└── src/
    ├── lib.rs
    ├── error.rs
    ├── api/             # 共同契约，单 PR 只能有一名编辑者
    ├── disk/            # A
    ├── io/              # A
    ├── buffer/          # A
    ├── superblock/      # A
    ├── alloc/           # A
    ├── journal/         # A
    ├── inode/           # B
    ├── extent/          # B
    ├── dir/             # B
    ├── file/            # B
    ├── writeback/       # B
    ├── xattr/           # B
    ├── vfs/             # B
    └── tests/
        ├── storage/     # A
        ├── semantics/   # B
        └── integration/ # 当次功能负责人
```

`src/api/` 不表示第三个实现层。它只存放跨所有权边界所需的窄接口、共享 ID
类型和错误类型，禁止放入业务算法。

## 关键数据流

### Buffered read

```text
KFS FileMapping miss
  -> B: inode logical page mapping
  -> B: 判断 hole/unwritten/mapped
  -> A: 提交连续数据块读取
  -> KFS: 页面变为 READY
```

hole 和 unwritten extent 必须返回零，不能读取未初始化磁盘内容。

### Buffered writeback

```text
KFS 收集连续脏页
  -> B: 建立 delayed-allocation 请求
  -> A: 开始 transaction 并保留 credits
  -> A: 分配连续物理块
  -> B: 更新 extent tree 和 inode
  -> A: 将修改过的 metadata buffer 加入 transaction
  -> B/A: 提交聚合数据 I/O
  -> A: ordered transaction 等待数据完成
  -> A: journal commit
```

数据 I/O 完成前，ordered 模式不能提交会暴露新数据块的元数据。

### Namespace mutation

create、unlink 和 rename 的共同流程是：

1. B 估算 transaction credits 和需要锁定的 inode 集合；
2. B 按 `locking.md` 获取 inode/目录锁，并通过 A 创建 transaction；
3. B 修改目录、inode、link count 或 orphan 状态；
4. A 分配或释放 inode/block，并 journal 所有 metadata buffer；
5. B 更新内存状态；
6. 释放业务锁后，由 A 决定 transaction commit 时机。

### Fsync

fsync 至少保证：

1. 目标 inode 范围内的脏数据已经完成写入；
2. 暴露这些数据所需的 extent、inode 和目录依赖已进入 journal；
3. 相关 transaction 已 commit；
4. 必要时底层设备已执行 flush/barrier。

具体语义由 `api.md` 定义，禁止把“清空某个本地 cache”等同于 fsync 完成。

## 并发模型

KExt4 禁止使用一把 `Mutex<Ext4State>` 串行所有操作。

并发状态按对象拆分：

- mount/superblock 生命周期状态；
- 每 inode 的 metadata/data/namespace 状态；
- 每 block group 的 allocator 状态；
- 每 metadata buffer 的内容和 I/O 状态；
- journal running/committing/checkpoint transaction；
- KFS 每 mapping 和每 cached page 状态。

完整锁顺序和 I/O 等待规则见 `locking.md`。

## 性能设计

性能目标不是通过省略一致性保证获得。核心策略包括：

- PageCache 作为唯一普通文件数据缓存；
- delayed allocation 和 extent-based multiblock allocation；
- 连续脏页聚合和批量 block I/O；
- `u64` bitmap 扫描、`first_hint`、per-group buddy；
- per-inode/per-group 锁，避免全局文件系统锁；
- metadata buffer 去重，避免每次访问完整反序列化和复制；
- transaction 合并，避免每次系统调用单独 commit；
- 热路径禁止无界线性扫描和不必要分配。

基准、回归阈值和对比方式见 `features.md`。

## 协作与合并规则

### 文件所有权

- A 默认只修改 A 目录，B 默认只修改 B 目录。
- 修改对方目录前，必须在 issue/PR 中取得该目录所有者确认。
- `api/`、五份规划文档和 workspace 配置属于共享文件；同一时间只能指定
  一名编辑者。
- 同一个功能涉及双方目录时，拆成“API PR、双方实现 PR、集成 PR”。

开始编码前，在共享 issue 中登记工作项：

| 字段 | 内容 |
|---|---|
| Work item | 唯一名称，例如 `journal-revoke` |
| Owner | A、B 或明确的功能负责人 |
| Writable paths | 本工作项允许修改的目录/文件 |
| Shared files | 需要预约的 `api/`、文档、`Cargo.toml`、`lib.rs` |
| Depends on | 已合并的 API PR 或其他工作项 |
| Integration PR | 最终由谁提交 |

同一路径同一时间只能出现在一个进行中的工作项中。开始另一个会修改相同共享
文件的工作项前，必须先合并或关闭前一个工作项。

`Cargo.toml`、`src/lib.rs`、`src/error.rs` 和 `src/api/` 使用“共享文件预约”：

- 预约者是该文件当前唯一编辑者；
- 预约应保持短期，只完成模块注册、导出或 API 变更；
- 另一人通过后续小 PR 接入，不能在自己的长期分支同时修改；
- 预约结束后立即同步共同开发分支。

### 公共 API 变更

公共 API 变更必须：

1. 先修改 `api.md`，说明动机、不变量和调用时序；
2. 单独提交 trait/type 变化，不夹带大规模实现；
3. 由双方 review；
4. 合并后，双方各自基于新接口实现；
5. 删除临时兼容层，不长期保留双接口。

### PR 粒度

一个 PR 应只包含下列一种主要变化：

- API 或数据模型；
- A 层实现；
- B 层实现；
- 集成和测试；
- 纯重构；
- 性能优化。

不得把格式化整个 crate、重命名公共类型和新增功能混在一个 PR 中。

### 分支和集成

- 共同集成分支保持可构建，不直接承载长期开发。
- 分支按所有者和工作项命名，例如 `kext4/a-journal-revoke`。
- 每个工作分支开始前 rebase 到最新共同分支，合并后另一人立即同步。
- 集成 PR 只连接已经 review 的 A/B 接口，不在集成时重新设计内部类型。
- 发生冲突时由目标文件所有者解决，功能负责人提供调用语义，不能由双方各自
  保留一份实现。

### 完成标准

功能完成需要同时满足：

- 正常路径测试；
- 边界和错误注入测试；
- 并发测试；
- 涉及持久化时的断电/重挂载测试；
- 与 Linux ext4 镜像互操作测试；
- 性能路径没有明显退化；
- `design.md`、`api.md`、`locking.md`、`features.md`、`vfs.md` 与代码一致。

## 非目标

KExt4 不要求完全复刻 Linux 内部类型或兼容 Linux 内核模块 API。复杂且对目标
工作负载价值有限的功能可以延后或明确不支持，但不能静默忽略磁盘上的
`INCOMPAT` feature。

功能取舍不能破坏：

- 磁盘格式校验；
- crash consistency；
- mmap 和 buffered I/O 一致性；
- fsync 持久化语义；
- 不泄漏未初始化磁盘数据；
- 不损坏未知 feature 的文件系统。
