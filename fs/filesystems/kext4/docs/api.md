# KExt4 - 跨层 API 契约

## 文档状态

本文定义成员 A 与成员 B 之间，以及 KExt4 与 KFS/BlockDevice 之间的接口
边界。代码尚未创建时，下面的 Rust 片段是契约草案，不是已经存在的 API。

接口名称允许在首次实现时调整，但所有权、错误语义和调用方向不得在未经双方
review 的情况下改变。

## API 分层

```text
外部上层接口：kvfs / KFS PageCache
                    |
                    v
       B: inode / extent / dir / writeback
                    |
             kext4::api 契约
                    |
                    v
       A: allocator / buffer / journal / I/O
                    |
                    v
             BlockDevice
```

## 共享基础类型

跨边界传递强类型 ID，避免把字节偏移、文件逻辑块和磁盘物理块混成裸 `u64`。

```rust
pub struct InodeNumber(pub u32);
pub struct LogicalBlock(pub u64);
pub struct PhysicalBlock(pub u64);
pub struct BlockCount(pub u32);
pub struct BlockGroupNumber(pub u32);
pub struct JournalCredits(pub u32);

pub struct PhysicalExtent {
    pub start: PhysicalBlock,
    pub len: BlockCount,
}
```

要求：

- 所有加减乘除使用 checked arithmetic；
- 字节偏移到块号的转换必须显式经过文件系统 block size；
- on-disk little-endian 类型不能直接作为运行时 ID 使用；
- 公共类型不暴露内部锁、裸指针或磁盘 buffer 地址。

## 错误模型

KExt4 内部使用自己的错误类型，VFS 边界统一转换为 `VfsError`。

建议错误分类：

```rust
pub enum Ext4Error {
    Io,
    Corrupt,
    Checksum,
    UnsupportedFeature,
    NoSpace,
    NoInode,
    JournalAborted,
    ReadOnly,
    Retry,
    InvalidArgument,
    Overflow,
}
```

规则：

- 磁盘格式错误返回 `Corrupt`，不得 panic；
- 不支持的 `INCOMPAT` feature 返回 `UnsupportedFeature` 并拒绝挂载；
- 一致性关键 I/O 失败应 abort journal，并按挂载策略只读降级；
- `Retry` 只用于调用者可以安全重试且没有部分可见副作用的情况；
- VFS 转换不能丢失 `NoSpace`、只读、损坏和 I/O 错误的区别。

## A 向 B 提供的接口

### Metadata buffer

metadata buffer 是一个文件系统块的唯一内存缓存身份。

```rust
pub trait MetadataStore: Send + Sync {
    fn read(
        &self,
        block: PhysicalBlock,
    ) -> Ext4Result<MetadataBuffer>;

    fn read_many(
        &self,
        range: PhysicalExtent,
    ) -> Ext4Result<Vec<MetadataBuffer>>;
}
```

`MetadataBuffer` 必须提供受控访问，而不是公开内部 `&mut [u8]` 的长期借用。
修改使用显式 write guard，避免 closure 已经改写部分字节后返回普通错误而无法
定义回滚语义：

```rust
impl MetadataBuffer {
    pub fn read(&self) -> MetadataReadGuard<'_>;

    pub fn write<'a>(
        &'a self,
        tx: &'a mut TransactionHandle<'_>,
    ) -> Ext4Result<MetadataWriteGuard<'a>>;
}

impl MetadataWriteGuard<'_> {
    pub fn bytes(&mut self) -> &mut [u8];

    pub fn finish(
        self,
        finalizer: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<()>;
}
```

不变量：

- 同一 `(filesystem, physical block)` 只有一个 buffer identity；
- 元数据修改必须持有 transaction handle；
- `write` 负责 journal write access、credits 和短期内容锁；
- 调用者在第一次写之前完成所有可能失败的解析和范围检查；
- `finish` 完成对象 checksum 并把 buffer 标记为 transaction dirty；
- 修改开始后的失败必须 abort transaction，不能伪装成已回滚；
- B 可以解释 extent/dir/inode 格式，但不能自行写回 buffer；
- buffer 生命周期不能超过所属文件系统实例。

### Transaction

```rust
pub trait Journal: Send + Sync {
    fn begin(
        &self,
        credits: JournalCredits,
    ) -> Ext4Result<TransactionHandle<'_>>;

    fn force_commit(&self, transaction_id: u64) -> Ext4Result<()>;
    fn wait_checkpoint(&self, transaction_id: u64) -> Ext4Result<()>;
    fn is_aborted(&self) -> bool;
}
```

`TransactionHandle` 至少支持：

```rust
impl TransactionHandle<'_> {
    pub fn id(&self) -> u64;
    pub fn reserve_more(&mut self, credits: JournalCredits) -> Ext4Result<()>;
    pub fn register_ordered_data(&mut self, completion: DataIoCompletion);
    pub fn mark_inode_sync(&mut self, inode: InodeNumber);
}
```

约束：

- transaction handle 不是跨线程共享锁；
- 持有 handle 不代表 transaction 已 commit；
- handle drop 只结束当前操作对 transaction 的使用，不能隐式声明 fsync 完成；
- ordered 模式下，commit 必须等待已注册的数据 I/O；
- credits 不足必须显式扩展或重启操作，不允许越界修改 metadata。

### Allocator

```rust
pub struct AllocationRequest {
    pub inode: InodeNumber,
    pub logical: LogicalBlock,
    pub len: BlockCount,
    pub goal: Option<PhysicalBlock>,
    pub flags: AllocationFlags,
}

pub trait SpaceAllocator: Send + Sync {
    fn allocate_blocks(
        &self,
        tx: &mut TransactionHandle<'_>,
        request: AllocationRequest,
    ) -> Ext4Result<PhysicalExtent>;

    fn free_blocks(
        &self,
        tx: &mut TransactionHandle<'_>,
        extent: PhysicalExtent,
    ) -> Ext4Result<()>;

    fn allocate_inode(
        &self,
        tx: &mut TransactionHandle<'_>,
        parent: InodeNumber,
        kind: InodeKind,
    ) -> Ext4Result<InodeNumber>;

    fn free_inode(
        &self,
        tx: &mut TransactionHandle<'_>,
        inode: InodeNumber,
    ) -> Ext4Result<()>;
}
```

约束：

- allocator 原子维护 bitmap、group descriptor 和 superblock free count；
- B 不自行补写计数器；
- 分配结果可能短于请求，B 必须正确处理；
- allocator 不更新 extent tree，也不理解目录语义；
- 释放前由 B 保证 extent 已从 inode 映射中移除或处于恢复协议中。

### Data I/O

文件数据不进入 metadata buffer cache，但批量 I/O 由统一接口提交：

```rust
pub trait DataIo: Send + Sync {
    fn read_pages(
        &self,
        runs: &[DataReadRun<'_>],
    ) -> Ext4Result<()>;

    fn write_pages(
        &self,
        runs: &[DataWriteRun<'_>],
    ) -> Ext4Result<DataIoCompletion>;

    fn flush_device(&self) -> Ext4Result<()>;
}
```

要求：

- 接口接受连续 run，不能强制 4 KiB 单次 I/O；
- completion 必须能表达提交失败和完成失败；
- writeback 页面在 completion 成功前保持 writeback 状态；
- A 负责块设备约束，B 负责页面生命周期和逻辑映射；
- metadata I/O 不得通过普通 `DataIo::write_pages` 绕过 journal。

## B 向 A 提供的信息

A 不反向依赖 B 的模块。需要 ordered-data 等待时，只登记通用完成对象：

```rust
pub struct DataIoCompletion {
    // 私有状态，由 DataIo 创建，Journal 只等待结果。
}

impl DataIoCompletion {
    pub fn wait(&self) -> Ext4Result<()>;
}
```

Journal 只能等待 completion，不得访问 `CachedFile`、`FileMapping`、
`Ext4Inode` 或 extent tree。

## B 内部接口

### Inode identity

同一挂载实例、同一 inode number 必须映射到唯一内存 inode identity：

```text
(filesystem instance, inode number) -> Arc<Ext4Inode>
```

硬链接产生不同 dentry，但必须共享 `Ext4Inode`、extent 状态和 KFS
`FileMapping`。

### Extent mapping

建议统一返回：

```rust
pub enum Mapping {
    Hole { len: BlockCount },
    Delayed { len: BlockCount },
    Unwritten { physical: PhysicalExtent },
    Mapped { physical: PhysicalExtent },
}
```

要求：

- read 对 `Hole`、`Delayed` 和未完成转换的 `Unwritten` 返回零；
- writeback 可以把 `Delayed` 转为 `Unwritten`，数据完成后再转 `Mapped`；
- extent 更新必须在 transaction 中修改 metadata buffer；
- 查询接口不得隐式分配块；
- truncate 必须使 PageCache、extent 和 inode size 按规定顺序失效。

## KFS / PageCache 外部接口

当前 KFS 仍通过 `FileNodeOps::read_at/write_at` 进行页面填充和回写。最终接口
应演进为 inode/address-space 级批量操作，而不是 KExt4 私有旁路。

完整的 VFS、PageCache、mmap、sync 和迁移规划见 `vfs.md`。

目标能力：

```rust
pub trait AddressSpaceOps {
    fn read_pages(&self, pages: &mut [PageReadRequest<'_>]) -> VfsResult<()>;
    fn write_pages(&self, pages: &mut [PageWriteRequest<'_>]) -> VfsResult<()>;
    fn invalidate_pages(&self, range: PageRange) -> VfsResult<()>;
    fn sync_pages(&self, range: PageRange, mode: SyncMode) -> VfsResult<()>;
}
```

演进规则：

- 该 trait 应位于 KFS/kvfs 合适的公共层，不放在 KExt4 中；
- rsext4 兼容适配和 KExt4 实现可以并存；
- PageCache 保持文件数据页所有者；
- KExt4 返回逻辑映射并执行文件系统相关的块分配和写回；
- mmap、buffered I/O 和 fsync 必须走同一 mapping identity。

## Fsync 契约

### `fdatasync`

保证文件数据及读取这些数据所必需的元数据持久化，例如 inode size 和 extent。
不要求持久化与数据无关的时间戳。

### `fsync`

在 `fdatasync` 基础上，还保证该 inode 的相关元数据更新按支持范围持久化。
新建文件是否需要父目录 fsync，遵循 Linux ext4 可观察语义并由测试固定。

### 完成条件

接口返回成功前必须满足：

- PageCache 目标脏页完成写入；
- ordered-data completion 成功；
- 相关 transaction commit 完成；
- 必要的设备 flush/barrier 完成；
- 延迟 I/O 错误已返回，不得静默清除。

## Mount 和 feature 协商

挂载流程必须先解析并验证：

- magic、block size、inode size；
- `compat`、`ro_compat`、`incompat` feature；
- descriptor size 和 checksum；
- journal 状态；
- device 容量与所有磁盘范围。

feature 处理规则：

| Feature 类型 | 不支持时行为 |
|---|---|
| `compat` | 可在明确安全时忽略，否则拒绝 |
| `ro_compat` | 只读挂载或拒绝 |
| `incompat` | 拒绝挂载 |

详细矩阵见 `features.md`。

## API 变更流程

任何跨 A/B 边界的 API 变更必须在 PR 描述中回答：

1. 哪个状态的所有权发生变化？
2. 调用方向是否仍然单向？
3. 锁顺序是否变化？
4. 错误发生时是否可能部分完成？
5. crash recovery 如何识别该状态？
6. 是否引入第二份缓存或重复计数？
7. 旧调用者如何迁移，临时兼容层何时删除？

接口 PR 合并前不得同时合并依赖该未稳定接口的两套不同实现。
