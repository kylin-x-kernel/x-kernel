# tipc — 安全与可靠性分析

## 信任模型

```text
core/ksyscall / tipc::syscall / kernel TIPC users
        │
        │ validated Rust values, Arc<dyn Handle>
        ▼
┌────────────────────────────────────────────┐
│ tee/tipc                                   │
│                                            │
│  global service registry                   │
│  port/channel/message state machines       │
│  memref metadata handle                    │
└────────────────────────────────────────────┘
        │
        ▼
tipc-handle / ktask / kspin / alloc
```

- `tipc::syscall` 负责 Trusty syscall ABI 的用户指针访问、ABI record 拷贝、syscall 编号分发和 errno 映射。
- 内核直接调用者负责在进入 core API 前提供已验证的 Rust 值。
- 本 crate 负责维护 TIPC 对象生命周期、连接状态、消息队列边界、handle 所有权和事件通知不变量。
- service path、UUID、队列大小、消息内容、attached handles 和 memref 元数据都应视为来自不可信或半可信调用方。
- `IpcUuid::default()` 当前被用作 non-secure client 判断，其它 UUID 被视为 TA client。真实平台身份和访问策略仍需要后续 TEE/NS client 集成补强。

## 外部边界 / 攻击面

| 边界 | 来源 | 进入 `tipc` 的形式 | 约束 |
|------|------|---------------------|------|
| service path | syscall adapter 或内核调用者 | `String` / `&str` | 非空、不能包含 NUL、长度小于 `IPC_PORT_PATH_MAX` |
| port queue sizing | service 创建者 | `num_recv_bufs`、`recv_buf_size` | 非零，分别不超过 `IPC_CHAN_MAX_BUFS` 和 `IPC_CHAN_MAX_BUF_SIZE` |
| connect flags | client | `IpcConnectFlags` | 只能包含 `WAIT_FOR_PORT` 和 `ASYNC` |
| port access flags | service 创建者 | `IpcPortFlags` | 只能包含 `ALLOW_TA_CONNECT` 和 `ALLOW_NS_CONNECT` |
| message bytes | channel 发送方 | `&[u8]` / user iovecs | core 发送路径拒绝超过队列 slot 的 slice；syscall iovec 入口按 peer receive slot 截断拷贝 |
| attached handles | channel 发送方 | `Arc<dyn Handle>` 切片 | 数量不能超过 `IPC_MAX_MSG_HANDLES`，且 handle 必须 `is_sendable()` |
| memref metadata | syscall adapter 或内核调用者 | address、size、protection bits | size 非零，address/size 页对齐，地址范围不能溢出，权限位必须在 mask 内，至少包含一个权限位，拒绝 exec 和 write-only；syscall 创建时要求当前进程 VMA 覆盖该范围并具备请求的 READ/WRITE 用户权限 |

仅 `syscall` 模块处理用户指针；core TIPC 路径不直接处理 MMIO、PIO、DMA、device tree、ACPI、FFI 或 inline assembly。

## unsafe 代码清单

`tee/tipc/src` 当前没有手写 `unsafe` 块或 `unsafe impl`。
`syscall` 模块使用 `posix_types` 的 `UserRead` / `UserWrite` derive 宏为 Trusty syscall ABI carrier 生成 marker impl。

需要持续保持的审计要求：

- 不用 raw pointer 表达 handle 或 channel 所有权；
- 不用 unchecked downcast；
- 不在 message queue 中保存裸用户地址；
- 不在 memref 中直接建立未经验证的内核映射。

## 内存安全不变量

1. **对象生命周期由 `Arc`/`Weak` 管理**：port、channel、handle set 和 memref 通过 `Arc<dyn Handle>` 或具体 `Arc<T>` 持有；registry 和 peer link 使用 `Weak` 防止全局表或双向 channel 环阻止释放。
2. **registry 不拥有 port**：全局 `PortRegistry` 只保存 weak port，lookup 时必须 upgrade 成功才使用。
3. **channel peer 访问需 upgrade**：所有对端访问都先升级 weak peer，失败返回 `NotConnected`。
4. **message slot 状态受队列锁保护**：`IpcMsgQueue` 只在 `IpcChan::msg_queue` 锁内访问，slot 状态必须按 `Free -> Filled -> Read -> Free` 转换。
5. **read slot 才能读取**：`ipc_read_msg` 和 `ipc_read_msg_handles` 只接受 `Read` 状态的 slot id。
6. **attached handles 使用强引用**：消息队列保存 `Arc<dyn Handle>`，直到接收方 `put` slot 才释放。
7. **handle table 关闭先脱离 handle set**：`tipc-handle::HandleTable::uctx_handle_remove` 在关闭目标 handle 前移除同 table 内 handle set 的 stale registration。
8. **memref 不解引用地址**：`sys_memref_create` 先验证调用方地址范围在当前进程地址空间内可访问；`MemRef` 只保存通过校验的地址范围和权限位，不直接访问该地址。

## 线程安全

| 状态 | 并发保护 | 风险控制 |
|------|----------|----------|
| 全局 port registry | `SpinNoIrq<PortRegistryState>` | path 发布、查找和 waiting list 更新串行化 |
| port state and pending queue | `SpinNoIrq<IpcPortInner>` | publish/attach/accept/close 对 lifecycle state 和 pending 队列的访问串行化 |
| channel state | `AtomicU8` | Acquire/Release 发布生命周期变化 |
| channel aux events | `AtomicU32` / `AtomicBool` | READY、HUP、SEND_UNBLOCKED 等事件跨线程可见 |
| channel peer and queue | `SpinNoIrq` | peer weak link 和 receive queue 访问串行化 |
| handle set entries | `tipc-handle` 内部 `SpinNoIrq<BTreeMap<...>>` | ctrl、poll 和 close 串行化 |
| handle cookie | `tipc-handle` 内部 `AtomicUsize` | caller cookie 的无锁读写 |
| handle table | 调用方外部锁 | `tipc-handle::HandleTable` 本身不是并发容器，通常由 process state 的 `RwLock` 包装 |

当前锁设计不适合长时间阻塞或中断上下文调用。
新增路径如果在 IRQ 或 early boot 上下文使用 TIPC，需要重新审计锁和 allocator 假设。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 恶意 service path 造成 registry 混淆 | 中 | 空 path、含 NUL、超长 path | `validate_port_path` 拒绝非法 path |
| T-02 | 重复发布同名 live port | 中 | 两个服务使用同一路径 | publish 时检查 live weak port，返回 `AlreadyExists` |
| T-03 | stale weak entry 造成连接到已释放 port | 中 | port drop 后 registry 尚未清理 | lookup/publish 时必须 upgrade，stale entry 会被忽略或清理 |
| T-04 | 未授权 client 连接服务 | 高 | port flags 与 client identity 不匹配 | `ipc_port_check_access` 基于 TA/NS flags 拒绝；真实身份集成仍是已知限制 |
| T-05 | 消息过大耗尽队列或覆盖 slot | 高 | 发送方传入超过 slot 大小的数据 | `push` 检查 `data.len() <= item_sz`；syscall 入口最多复制 peer receive slot 大小 |
| T-06 | 队列满导致发送方忙等 | 中 | 接收方不 `put` slot | 返回 `WouldBlock`，释放 slot 后通知 `SEND_UNBLOCKED` |
| T-07 | handle set 持有已关闭 handle id | 中 | handle 关闭但仍注册在 handle set | `HandleTable::uctx_handle_remove` 调用 `detach_from_handle_sets` |
| T-08 | handle set 循环导致 poll 递归或死循环 | 高 | handle set 嵌套注册 | 当前直接拒绝注册 `HandleSet` |
| T-09 | memref 元数据表示越界、非页对齐或未映射地址范围 | 中 | `addr + size` 溢出、范围不满足 Trusty memref 页粒度、或 syscall 调用方试图引用未覆盖/权限不足的 VMA | `MemRef::create` 使用 `checked_add` 并要求 address/size 页对齐；`sys_memref_create` 使用当前进程地址空间校验 VMA 覆盖和 READ/WRITE 权限 |
| T-10 | 不可发送 handle 被跨 channel 传递 | 中 | 发送方附带 handle set 等对象 | `IpcMsgQueue::push` 检查 `is_sendable()` |
| T-11 | close/drop 重入导致状态不一致 | 中 | 多路径重复关闭 port/channel | `close` 设计为幂等，状态切换后通知等待者 |
| T-12 | vsock-TIPC bridge 把 object capability 透传给 host | 高 | TIPC message 附带 handle 或 memref | bridge v1 只转发 bytes，发现 attached handles 时关闭连接 |
| T-13 | host 通过动态 port 0 连接非法 TIPC service path | 中 | 首个 vsock record 为空、含 NUL、超长或非 UTF-8 | bridge 在连接 TIPC 前校验 service name，并复用 TIPC path validation |

影响等级定义：

- 高：可能导致权限绕过、跨进程对象能力错误暴露或内存安全风险。
- 中：可能导致 IPC 语义错误、拒绝服务、资源泄漏或 wait/poll 错误。
- 低：短暂查询失败、事件延迟或统计不一致。

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `ipc_port_connect_async` 返回 `NotFound` | port 未发布且未设置 `WAIT_FOR_PORT` | client 连接失败 | TA 服务发现失败 | 3 | 调用方按 Trusty 语义选择是否等待 port |
| F-02 | waiting client 无法 attach | client 在 port 发布前关闭或访问被拒绝 | 单个等待连接失败 | 该 client 连接失败 | 3 | publish 时跳过 stale weak，attach 失败则关闭 client |
| F-03 | `ipc_port_accept` 返回 `WouldBlock` | pending queue 为空 | service accept 失败 | 服务端需等待 READY 事件 | 3 | port poll 在 pending 非空时返回 READY |
| F-04 | channel send 返回 `WouldBlock` | 对端接收队列已满 | 当前消息未发送 | 发送方需等待 SEND_UNBLOCKED | 3 | `put` 从 full 变 writable 时通知 peer |
| F-05 | message read 返回 `BadState` | 未先 `get` 或 slot 已 `put` | 读取失败 | 调用方协议错误 | 3 | slot 状态检查 |
| F-06 | handle id 复用失败 | `i32` id 空间耗尽 | 新 handle 安装失败 | 进程 TIPC 能力耗尽 | 2 | 完整环形扫描后返回 `TooManyOpenFiles` |
| F-07 | handle set poll 返回 `NotFound` | handle set 为空 | wait 失败 | 调用方需重新注册目标 handle | 4 | 明确区分空集合和暂未就绪 |
| F-08 | memref 无法映射到接收方地址空间 | 当前只保存元数据 | memref 只能传递元数据 | 完整共享内存语义不可用 | 3 | 后续接入 VMM/mmap 语义 |

严重度定义：

- 1：致命，可能导致内存破坏或权限提升。
- 2：严重，导致 TIPC 子系统或进程能力表不可用。
- 3：一般，单个连接、消息或 handle 操作失败。
- 4：轻微，可由调用方重试或重新注册恢复。

## 故障管理

- 参数错误使用 `KResult` 返回 `InvalidInput`、`OutOfRange`、`NameTooLong` 等 typed error。
- 队列暂不可用使用 `WouldBlock`，由调用方结合 poll/wait 处理。
- 对端关闭或 peer weak upgrade 失败返回 `NotConnected`。
- stale registry entry 不 panic，publish/connect 路径会忽略或清理。
- close/drop 路径唤醒等待者，使 poll/wait 能观察 ERROR/HUP。
- `debug_assert!` 仅用于内部状态自检，不作为外部输入防线。

## 隐私分析

`tipc` 保存 service path、caller UUID、message bytes、attached handle 强引用、handle cookie 和 memref 地址元数据。
这些数据可通过 syscall 层或内核调用者被同一连接对端观察。

本 crate 不负责访问控制以外的隐私策略：

- service path 命名约定由上层服务定义；
- UUID 真实性由 TEE/NS client 集成提供；
- message payload 是否包含敏感信息由通信双方协议约束；
- memref 地址值当前只是元数据，但仍可能泄露发送方虚拟地址布局，应由 syscall/TEE policy 决定是否允许暴露。

## 已知限制

- `IpcUuid::default()` 暂作为 NS client 判定，尚未接入 Trusty `is_ns_client` 等真实身份路径。
- access policy 只支持 TA/NS 两类 flag，未实现完整 `tipc_config` 策略。
- `MemRef` 只保存经过 syscall VMA 覆盖/权限校验的地址、大小和权限位，尚未绑定 VMM 对象，也不支持接收方 mmap；`validate_mmap` 仅提前固化 offset/size/权限校验语义。
- `HandleSet` 当前禁止嵌套；Trusty 支持非循环嵌套。
- `HandleTable` 是 `tipc-handle` 的 process-local 数据结构，但锁由调用方提供。
- kernel internal 调用方直接使用 `ipc_port_*`、`IpcChan` 和 `HandleSet` 对象 API；尚未提供单独的 `ktipc` facade。
- vsock-TIPC bridge v1 只转发 message bytes，不转发 handles 或 memrefs。

## 审计清单

修改本模块时需验证：

- 新增 public API 的 rustdoc 描述错误返回、阻塞语义和对象生命周期。
- 新增用户输入字段在 syscall adapter 或本 crate 边界完成长度、范围和 flag 校验。
- 新增 `unsafe` 必须有 `SAFETY:` 注释，并更新本文件的 unsafe 清单。
- 新增 registry 操作不在全局锁内执行可能阻塞或复杂的跨对象操作。
- 新增 channel/message 操作保持 slot 状态机和 queue lock 不变量。
- 新增 handle 类型明确 `is_sendable`、`poll`、`close` 和 `cookie` 语义。
- 新增 handle set 能力时重新审计循环、stale handle id 和 close detach 行为。
- 新增 memref/VMM 绑定时审计地址空间权限、生命周期、映射撤销和跨进程信息泄露。
- 新增 IRQ/early boot 调用点时重新审计 allocator、scheduler、waker 和 spin lock 假设。
