# memfs — 设计文档
## 定位

`memfs` 提供纯内存文件系统的 inode 和目录树实现。普通文件内容通过
`VfsInode::i_mapping -> kvfs::AddressSpace -> pagecache::Mapping` 提供
cached content 与 MM shared object identity。

Linux 对应关系：

- tmpfs/shmem inode 持有 `address_space`
- `mm/shmem.c` 通过 inode-backed page cache 管理文件内容
- `shmem_file_setup()` 创建不挂入全局路径空间的匿名 tmpfs 文件对象

## 背景

`memfs` 只拥有 inode、目录树、符号链接和元数据。普通文件的页缓存、evict
listener 和 MM object identity 归 VFS inode address-space 管理。
因此同一 inode 的 open-file、hard-link、mmap 和 truncate 路径共享同一个内容
owner，而不是由每个 dentry 或 open file 单独拥有内容对象。

## 范围

- `src/lib.rs`
- `src/shmem.rs`
- regular-file inode 与 VFS address-space/page-cache 的连接
- 目录、链接、符号链接和元数据维护
- fd-only tmpfs/shmem anonymous regular-file factory and opened-file conversion

## 架构

```text
MemoryFs
  -> Inode / VfsInode
       -> i_mapping: kvfs::AddressSpace
            -> page_cache: pagecache::Mapping

File::page_cache_mapping()
  -> VfsInode::i_mapping / AddressSpace
  -> pagecache::Mapping
```

`memfs` 自身不定义第二套 file-backed content owner。KFS 高层通过
`File::page_cache_mapping()` 进入文件缓存路径后，只取得 inode address-space
mapping；page cache、evict listener 与 MM shared object identity 的统一宿主是
`VfsInode::i_mapping` 下的 `AddressSpace`。

## 调用约束 / 执行上下文

- regular-file read/write/truncate 允许睡眠，因为可能分配 folio。
- 目录和元数据操作使用 `memfs` 自身锁保护。
- 不适用于中断上下文。

## 算法流程

### 创建普通文件

1. `memfs` 创建 regular-file inode。
2. `VfsInode` 构造时持有稳定的 `kvfs::AddressSpace`。
3. 首次通过 KFS page-cache file path 进入文件缓存路径。
4. `AddressSpace` 建立或复用唯一的 inode-owned `pagecache::Mapping`。
5. open-file、mmap、truncate 和 evict 路径复用同一个 address-space mapping。

### 创建匿名文件

1. `memfs::shmem` factory 创建私有 `MemoryFs`，名称为 `tmpfs`。
2. factory 在该私有 filesystem 的 root mount 下创建 regular file。
3. file inode 仍通过同一个 `VfsInode::i_mapping` 获取 page cache 与 MM object
   identity。
4. 返回的 `Location` 由调用方通过 KFS `OpenOptions` 打开成 fd，不挂入进程可见
   路径空间。

### 读取普通文件

1. KFS file path 找到 inode address-space mapping。
2. `pagecache::Mapping` 读取或 materialize folio。
3. 洞页返回零填充。

### 写入普通文件

1. KFS file path 找到 inode address-space mapping。
2. `pagecache::Mapping` 写入 folio 并标记 dirty。
3. inode-owned `Mapping` 继续提供共享 object identity 与 mmap contract。

## 并发模型

- inode metadata 由 `Mutex<Metadata>` 保护。
- 目录项表由 `Mutex<HashMap<...>>` 保护。
- inode address-space page cache、evict listener 与 MM object identity 由
  `pagecache::Mapping` 内部同步保护。

## 设计决策

1. `memfs` 不自行实现 file-backed content owner，而是复用 VFS inode
   address-space mapping。
   原因：同一 inode 的 file I/O、mmap、truncate 和 hard-link alias 必须共享同一
   内容对象。

2. 符号链接仍由 `FileContent.symlink` 保存。
   原因：symlink target 是小字符串元数据，不需要进入 page cache。

3. 匿名文件用私有 tmpfs mount 中的 regular file 表达。
   原因：这保持了 Linux `shmem_file_setup()` 的私有文件对象 + inode-owned page
   cache 语义，同时不把对象挂入进程可见路径空间。调用方通过 shmem 对象转换成
   opened `VfsFile`，不重新操作匿名路径的 open 细节。

## Drop / 资源释放

- inode 释放时，`AddressSpace` 与其 `pagecache::Mapping` 随引用计数释放。
- 目录删除逻辑由 `InodeRef` 和 nlink 维护。
