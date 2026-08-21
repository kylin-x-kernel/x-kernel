# anon_inodefs — 安全与可靠性分析

## 信任模型

filesystem type、operation tables 和 boot 初始化代码是受信任内核代码。调用者提供的
file operation object、typed private object、名称、flags 和 credential 都已位于内核
地址空间，但名称和 flags 仍不能被当作文件系统结构或权限来源。

## 外部边界 / 攻击面

- `name` 会进入 proc-style 动态路径显示，但只作为内核拥有的 Rust `str` 格式化。
- raw syscall flags 不直接进入本 crate；调用者传入类型化 `OpenFlags`，本 crate 只保留
  access mode 与 nonblocking bits。
- `Cred` 只作为 open-time credential 保存，不在这里执行 pathname DAC。
- caller-provided `FileOperations` 可能在 file open/release/I/O 路径失败或阻塞。

本 crate 不访问用户指针、MMIO/PIO、DMA、设备输入、固件、FFI 或汇编。

## unsafe 代码清单

`src/lib.rs` 没有 `unsafe` block。

## 内存安全不变量

- `AnonInodeFs::global()` 只能在 `Once` 完整发布 mount 与 singleton inode 后返回。
- 所有普通匿名 file 必须引用同一个 singleton inode，不建立平行 inode identity。
- file private data 由 `Arc` 持有，必须满足 `Any + Send + Sync + 'static`。
- dentry operations 从 superblock 的静态默认 table 继承，dentry 绑定后不得替换。
- 动态名称只读取 dentry 自己的稳定 name snapshot。

## 线程安全

初始化预期由 boot CPU 串行调用，`Once` 仍保证重复调用幂等。发布后对象字段只读；
并发 file 创建由 KVFS 内部锁和 `Arc` 管理，不存在本 crate 自有可变全局状态。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 未初始化访问隐藏 mount | 高 | boot 漏掉 `init_anon_inodefs()` | `global()` 立即 panic，在启动/测试阶段暴露顺序错误 |
| T-02 | 每个 file 分配独立 inode 或 operation state | 中 | caller 绕过 singleton 或 filesystem 保存 per-file table | 唯一公开创建入口复用 singleton inode；operation tables 为静态/共享对象 |
| T-03 | 不受支持的 open flags 泄漏到 file 状态 | 中 | caller 混入 create/path/exec flags | `get_file()` 只保留 access mode 与 `NONBLOCK` |
| T-04 | dentry operation 与 superblock 不一致 | 中 | per-file 手动安装 `d_op` | dentry 从 superblock `s_d_op` 对应物自动继承，绑定后不可替换 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 全局对象未初始化 | boot wiring 缺失 | 当前创建 panic | 用户态启动前停止 | 2 | boot 显式初始化，单元测试覆盖初始化后创建 |
| F-02 | file 分配失败 | 内存不足或 VFS 校验失败 | 当前创建返回错误 | 无全局状态变化 | 3 | 通过 `VfsResult` 原样传播，private data 只在成功后安装 |
| F-03 | file release 失败 | caller fops cleanup 错误 | 当前 close 返回错误 | filesystem singleton 保持可用 | 3 | cleanup 归 caller operation table；共享 mount/inode 不随单 file teardown |

## 故障管理

可恢复的 file 分配和 open 错误通过 `VfsResult` 返回。只有违反 boot 初始化契约时 panic；
不在 runtime 首次访问时尝试隐式恢复或重新构造全局 VFS 对象。

## 隐私分析

本 crate 不读取 file private object 内容，也不复制用户数据。动态名称可能出现在 proc/debug
路径输出中，因此调用者应使用固定 kernel-object class 名称，不应嵌入敏感数据。

## 已知限制

- 尚未实现 Linux secure/unique anonymous inode 与 LSM security-init 路径。
- hidden filesystem 不支持运行期卸载。
- operation-table 存储仍遵循 KVFS inode/file operation API；本 crate 不建立额外缓存。

## 审计清单

- 新匿名 kernel object 是否使用 `AnonInodeFs::global().get_file()`？
- boot 是否在任何并发 caller 之前调用 `init_anon_inodefs()`？
- name 是否为固定 class label，未包含用户秘密？
- 新 flags 是否在 syscall 边界类型化，且未绕过 `get_file()` 的 mask？
- 是否错误增加 per-file inode、mount 或 dentry-operation owner？
