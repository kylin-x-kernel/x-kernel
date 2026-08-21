# pipefs — 安全与可靠性分析

## 信任模型

pipefs type、operation tables 和 boot wiring 是受信任内核代码。status flags 是 syscall
层完成组合检查后构造的 `OpenFlags`，credential 是调用时稳定 `Arc<Cred>`；本 crate
只保留 `NONBLOCK`，并自行派生只读端和只写端 access mode。

## 外部边界 / 攻击面

- pipe2 用户 flags 经 syscall 层收窄后影响两个 file 的 nonblocking 状态。
- credential 的 fsuid/fsgid 决定 pipe inode metadata owner。
- 后续用户 I/O、poll、ioctl 和 resize 进入 `kvfs::pipe`，本 crate 不接触用户指针。

本 crate 不访问 MMIO/PIO、DMA、设备/中断输入、固件、FFI 或架构汇编。

## unsafe 代码清单

`src/lib.rs` 没有 `unsafe` block。

## 内存安全不变量

- `PipeFs::global()` 只能在 hidden mount 完整发布后返回。
- 每次 `create_pipe_files()` 必须创建一个唯一 inode；同一 read/write pair 必须共享该
  inode 和同一个 `Arc<PipeObject>`。
- inode private、read-file private 和 write-file private 不得指向不同 pipe session。
- dentry 必须绑定 pipefs superblock 并继承静态默认 dentry operations。
- write file 创建失败时不得发布 read file；read clone 失败时局部对象由 `Arc` 回收。

## 线程安全

全局 mount 只读并由 `Once` 发布。并发创建只竞争 KVFS allocator/registry 内部同步；
不同 pipe session 没有共享可变状态。同一 pipe 的同步契约由 `PipeObject` 实现。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | pipe 错误复用 anon-inode singleton | 高 | 用 anon_inodefs 创建两个端点 | 独立 pipefs 每次创建唯一 FIFO inode |
| T-02 | 两个 file 指向不同 pipe state | 高 | clone 时重新分配 private object | inode/read/write 明确 clone 同一 `Arc<PipeObject>`，测试检查指针一致性 |
| T-03 | inode owner 固定 root | 中 | 忽略调用 credential | inode init 使用传入 `Cred::fsuid/fsgid()` |
| T-04 | 未初始化 pipefs 被 syscall 使用 | 高 | boot wiring 缺失 | `global()` panic，使错误在启动/测试期显式失败 |
| T-05 | dentry 名称泄漏错误 identity | 低 | 使用固定 `[pipe]` 或 anon dname | 静态 pipefs dentry table 从唯一 inode number 生成 `pipe:[ino]` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | pipefs 未初始化 | boot 顺序错误 | pipe 创建 panic | 用户态启动前停止或测试失败 | 2 | `fs_boot` 显式初始化 |
| F-02 | 调用者传入无关 status flags | 内核调用点没有收窄参数 | 无关位若传播会污染 file 状态 | fd 行为偏离 pipe2 | 3 | 类型化入口只保留 `NONBLOCK`，access mode 由 pipefs 固定派生 |
| F-03 | inode/file 分配失败 | 内存不足 | 当前创建失败 | 已分配局部状态自动释放 | 3 | `KResult` 传播，顺序发布两个 file |
| F-04 | read clone 失败 | file 分配失败 | write file 未返回 caller | pipe state 局部回收 | 3 | clone 成功后才返回 pair |

## 故障管理

输入和分配错误通过 `KResult` 返回；未初始化属于 boot 契约破坏并 panic。没有半初始化
global state 的重试路径，也不把失败 pipe 放入额外 registry。

## 隐私分析

pipefs 只保存 pipe bytes 的 owner object，不读取或记录其内容。动态名称只包含 inode
number，不包含 pipe payload、credential 或用户地址。

## 已知限制

- 尚未支持 Linux notification pipe 与 `O_DIRECT` packet mode。
- hidden pipefs 不支持运行期卸载。
- pipe resource accounting/limits 仍由当前 `kvfs::pipe` 能力决定。

## 审计清单

- 匿名 pipe 是否只通过 `pipefs::create_pipe_files()` 创建？
- 每次创建是否分配唯一 inode，而 read/write 是否共享 inode 和 `PipeObject`？
- owner 是否来自调用 credential，而不是固定 root 或重新读取 current task？
- dentry 是否显示 `pipe:[ino]`，且 operation table 来自 superblock 默认 `s_d_op`？
- 新错误路径是否在返回 fd 前完整释放局部 file/inode/pipe state？
