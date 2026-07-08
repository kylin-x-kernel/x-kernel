# kidentity — 安全与可靠性分析

## 信任模型

`kidentity` 信任其内核内调用者会在正确的 process-domain 场景中使用
`PidNamespace` 和 `PidHandle`，
但不信任调用者会自动满足更高层 publication 顺序语义。
crate 本身只保证编号分配与 namespace 链表达的一致性。

## 外部边界 / 攻击面

该 crate 不直接接收用户内存、设备输入、DMA、MMIO、文件系统或网络数据。
外部输入仅包括：

- 调用者传入的 `Arc<PidNamespace>`；
- 调用者要求固定 root 编号的 `fixed_root(nr)` 参数。

## unsafe 代码清单

当前 crate 不包含 `unsafe` 代码。

## 内存安全不变量

- `PidNamespace::parent` 一旦建立后不可变，
  不得形成可变共享下的层级破坏。
- `PidHandle::numbers` 中每个 `Upid` 必须绑定有效的 `Arc<PidNamespace>`。
- `PidHandle` 必须始终持有 root-visible 编号，
  因而 `root_nr()` 的 `expect` 依赖分配流程保持该不变量。

## 线程安全

- `PidNamespace` 通过 `AtomicU32` 支持并发分配。
- `PidHandle` 和 `Upid` 在构造后只读，
  线程安全由其字段类型自然保证。
- crate 不负责跨 subsystem 的 publication 原子性；
  该责任在更高层 owner crate。

## 威胁分析

- 编号耗尽：`u32` 空间耗尽会使后续分配失败。
  影响是新 task/process 无法获得 identity；
  当前缓解方式是显式返回 `KError::WouldBlock`。
- 发布顺序错误：如果上层在 identity 稳定前就让 task runnable，
  可能破坏 PID/TID 可观测一致性。
  该 crate 通过 ownership 分层把这项责任留给 `kprocess` 等 owner。
- 错误 namespace 查询：调用方若拿非祖先 namespace 调用 `nr_in()`，
  会得到 `None`，不会伪造编号。

## 故障模式与影响分析（FMEA）

| 故障模式 | 触发条件 | 局部影响 | 系统影响 | 处理方式 |
|---|---|---|---|---|
| 编号溢出 | `next_nr` 到达 `u32::MAX` | 当前 identity 分配失败 | 新建任务或进程失败 | 返回 `KError::WouldBlock` |
| namespace 链为空 root | 构造逻辑被破坏 | `root_nr()` panic | 内核逻辑错误暴露 | 依赖 `allocate_in` / `fixed_root` 保持不变量 |
| 使用错误 namespace 查询 | 调用方传入无关 namespace | 返回 `None` | 上层需决定错误处理 | 显式 `Option` 返回 |

## 故障管理

- 常规错误通过 `KResult` 返回。
- `root_nr()` 的 panic 表示内部不变量已经被破坏，
  不是对外部输入的可恢复错误路径。
- crate 不做重试、回滚或编号回收。

## 隐私分析

该 crate 不处理用户隐私数据，
只管理内核内的进程身份编号。

## 已知限制

- 当前不支持 PID reuse。
- `nr_in()` 是线性扫描。
- crate 不维护生命周期回收策略，也不负责 publication 事务。

## 审计清单

- 检查每次 `PidHandle` 分配后，上层是否在 runnable 前完成 publication。
- 检查新引入的 namespace 操作是否保持 root-visible 编号始终存在。
- 检查任何未来的 PID reuse 设计是否破坏当前只读 `PidHandle` 假设。
- 检查并发分配路径是否仍只依赖 `AtomicU32`，没有引入额外共享状态竞态。
