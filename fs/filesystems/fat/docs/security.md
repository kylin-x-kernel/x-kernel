# fat — 安全与可靠性分析

## 信任模型

FAT 介质内容和 block I/O 结果不可信。KVFS 可信地提供已解析的 canonical
`BlockDevice`、mount flags、VFS inode/dentry 生命周期；`axfatfs` 负责格式解析，本 crate
负责错误转换和锁域。

## 外部边界 / 攻击面

- FAT boot sector、allocation table、directory entry 和文件数据；
- block read/write/flush failure；
- KVFS lookup/create/read/write/metadata callbacks。

本 crate 不接触 user pointer、MMIO、PIO、DMA、FFI 或 interrupt handler。

## unsafe 代码清单

`src/lib.rs` 的 `FsRef` 把受 owner mutex 保护的 `axfatfs` file/dir handle lifetime 延长到
wrapper 生命周期，并通过 `UnsafeCell` 提供 guard-bound borrow。`from_file_handle()`、
`from_dir_handle()`、`borrow()`、`borrow_mut()` 及 `Send/Sync` 实现共同依赖以下不变量：

- `FatFilesystem` 由 `Arc` 固定存活，`FatFilesystemInner` 由 `PhantomPinned` 标记地址敏感；
- 每个 `FsRef` 保存创建它的 owner 地址，借用前由 `assert_owner()` 校验；
- matching filesystem mutex guard 覆盖每次访问，共享/独占借用由 guard 类型区分。

各 unsafe block、unsafe function 和 unsafe impl 均在代码旁记录对应 `SAFETY`/`# Safety`
合同。

## 内存安全不变量

- FAT handle 不得在 owner mount state 销毁后访问，也不得在另一个 mount 的 guard 下借用；
- mutable handle borrow 必须由唯一的 mutable guard 承载；
- root superblock 只能在格式解析成功后发布；
- `register_init` 回调只能注册一个静态 `FileSystemType`，root 与普通 mount 必须使用该对象；
- `fill_super` 只能给 KVFS 已分配的新生 `(s_type, dev_t)` superblock 安装私有 operations 与
  root；同设备已有 live instance 必须由 VFS 复用。

## 线程安全

所有 FAT handle operation 由 mount-wide mutex 串行化。`FsRef` 不直接向无 guard 调用者暴露
内部引用；KVFS 自身负责 dentry/inode/mount topology 的并发。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 损坏 FAT 元数据导致越界或 panic | 高 | 外部介质字段畸形 | `axfatfs` 解析错误经 `into_vfs_err` 返回；`fill_super` 不使用 `expect` |
| T-02 | handle 在错误 owner/lock 下访问 | 高 | wrapper 与 mount state 混用 | `FsRef` 保存 owner pointer，每次 borrow 校验 matching mutex guard |
| T-03 | root 与普通 mount 行为漂移 | 中 | 另建 root provider 或 callback | 自有 `register_init` 回调注册唯一静态 `FILE_SYSTEM_TYPE`，随后经 KVFS registry、`get_tree_bdev` 和 `fill_super` 分派 |
| T-04 | 同一 FAT 设备建立两套 mutable mount state | 高 | 每次 mount 都直接调用 `fill_super` | KVFS 在调用 FAT 前按 `(s_type, dev_t)` reservation；已有 live superblock 直接复用，RO/RW 不一致返回 `EBUSY` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | FAT 初始化失败 | 损坏格式或 I/O 错误 | `fill_super` 返回错误 | 当前 mount 失败；root 无候选时停止 boot | 2 | 传播 typed VFS error，不发布半初始化 superblock |
| F-02 | block flush 失败 | 后端设备错误 | 同步请求失败 | 持久化不保证 | 2 | 显式 sync/flush 路径传播错误 |
| F-03 | owner 校验失败 | 内部 handle 跨 mount 混用 | 内核 assertion 终止当前执行 | kernel panic | 1 | wrapper 构造绑定 owner；review 禁止绕过 `FsRef` borrow API |

## 故障管理

挂载解析和正常 I/O 错误通过 `VfsResult` 返回。内部 owner mismatch 表示类型合同破坏，使用
assertion 阻止产生错误引用。

## 隐私分析

本 crate 处理文件名、metadata 和文件内容，但不主动记录这些数据。

## 已知限制

mount-wide mutex 串行化 FAT 操作；FAT 无法完整表达 Unix owner/permission 语义。

## 审计清单

- 新 mount 能力是否仍经唯一 canonical `FileSystemType`？
- 新 FAT handle 是否只通过 matching owner guard 借用？
- 新 unsafe lifetime extension 是否列出 owner、pinning、锁和 aliasing 不变量？
- 格式或 I/O 错误是否返回而非 panic？
