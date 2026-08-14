# tipc — 设计文档

## 定位

`tipc` 是 X-Kernel 中 Trusty IPC 的传输无关核心 crate。
它负责维护 port、channel、message、memref 和全局 service registry。

`tipc-handle` 负责 `Handle` trait、`HandleSet` 和 process-local `HandleTable`，
供 `tipc` 与 `process/kprocess` 共同依赖。
`tipc` 同时承载 Trusty syscall ABI adapter；
`core/ksyscall` 只在总 syscall dispatch 中把 TIPC syscall number 转接到该 adapter。

## 背景

Trusty TIPC 以具名 port 建立双向 channel，并以 handle 作为对象能力传递。
X-Kernel 需要先提供一层不依赖具体 TEE runtime 和 syscall ABI 的 IPC 核心，使后续系统调用适配、TA 服务和 kernel-side helper 可以共享同一套状态机。

当前实现优先覆盖以下语义：

- service port 创建、发布、连接和 accept；
- client 可早于 port 发布而进入等待状态；
- channel 双端点、固定槽消息队列和消息边界；
- handle event、cookie、handle set 和 process-local handle table 由 `tipc-handle` 统一维护；
- handle 传递的基础所有权模型；
- memref handle 的 Trusty-visible 元数据和创建时用户映射校验。

## 范围

涉及的源文件：

```text
tee/tipc/
├── Cargo.toml
├── Kconfig
├── docs/
│   ├── design.md
│   ├── memref.md
│   └── security.md
└── src/
    ├── lib.rs
    ├── channel.rs
    ├── memref.rs
    ├── message.rs
    ├── port.rs
    ├── registry.rs
    ├── syscall.rs
    └── tests.rs
```

相关共享 handle crate：

```text
tee/tipc-handle/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── handle.rs
    ├── handle_set.rs
    └── handle_table.rs
```

## 架构

```text
core/ksyscall dispatch
        │
        │ feature "tipc"
        ▼
tee/tipc::syscall adapter
        │ copyin/copyout, sysno routing
        ▼
┌────────────────────────────────────────────┐
│ tee/tipc                                   │
│                                            │
│  PortRegistry                              │
│    ├─ ports: path -> Weak<IpcPort>         │
│    └─ waiting_for_port: path -> Weak<IpcChan>[] 
│                                            │
│  IpcPort                                   │
│    └─ inner: state + pending server queue  │
│                                            │
│  IpcChan pair                              │
│    ├─ client endpoint receive queue        │
│    └─ server endpoint receive queue        │
│                                            │
│  IpcMsgQueue                               │
│    ├─ free slots                           │
│    ├─ filled slots                         │
│    └─ read slots                           │
│                                            │
│  MemRef                                    │
└────────────────────────────────────────────┘
        │
        ▼
tee/tipc-handle::{Handle, HandleSet, HandleTable}
        ▲
        │
process/kprocess ProcessRuntime owns per-process HandleTable
```

| 组件 | 职责 |
|------|------|
| `IpcPort` | 表示服务端具名 port，保存发布状态、访问策略、接收队列配置和未 accept 的 server endpoint 队列 |
| `IpcChan` | 表示双向连接的一端，保存生命周期状态、对端 weak 引用、接收队列、事件状态和 cookie |
| `IpcMsgQueue` | 固定槽消息队列，保持 Trusty 消息边界和 `get/read/put` 三阶段读取模型 |
| `PortRegistry` | 全局服务命名空间，按 path 查找已发布 port，并保存等待 port 发布的 client endpoint |
| `tipc_handle::Handle` | port、channel、handle set、memref 的统一事件、关闭、cookie 和 downcast 接口 |
| `tipc_handle::HandleSet` | 多 handle 事件聚合器，按 process-local handle id 返回就绪事件 |
| `tipc_handle::HandleTable` | process-local integer id 到 `Arc<dyn Handle>` 的映射，负责 close 时从 handle set 脱离 |
| `MemRef` | 可通过消息传递的内存引用元数据；syscall 创建时验证当前进程映射，当前不绑定可 mmap 的 VMM 对象 |
| `syscall` | Trusty syscall ABI adapter，负责用户指针、syscall 参数和当前进程 handle table 挂接 |

## 调用约束 / 执行上下文

- 本 crate 是 `no_std` 内核 crate，依赖 `alloc`、`kspin`、`kpoll` 和 `ktask`。
- API 面向 task/syscall 生命周期路径，不应在中断上下文调用。
- `IpcChan::wait_connected` 会通过 `ktask::future::block_on` 等待连接完成，内部
  `PollRegistrations` 跨越 `Pending`；调用方必须处在允许阻塞的上下文。
  该等待可被 fatal signal 中断，此时返回 `KError::Interrupted`。
- `tipc_handle::Handle::register` 使用 `PollContext` 并可返回注册错误；poll/wait
  调用方需要位于调度器可用之后，并在每轮 poll 刷新 registration context。
- `tipc_handle::HandleTable` 本身不持锁，调用方负责把它放入 process-local 锁保护中。
- `syscall` 模块依赖 `kprocess`、`khal`、`linux_sysno` 和 `posix-types`，只能在用户线程 syscall 上下文使用。
- `PortRegistry` 是全局命名空间，路径、端口发布和 waiting client 操作由内部 `SpinNoIrq` 串行化。
- early boot 阶段不应创建 TIPC port 或 channel；该模块假设 allocator、scheduler 和基础同步设施已经可用。

## 状态机

### Port 生命周期

```text
Invalid
  ├─ mark_published() / ipc_port_publish()
  ▼
Listening
  ├─ port_attach_client() -> pending_list push
  ├─ ipc_port_accept()    -> pop pending server endpoint
  └─ close()/drop()
      ▼
Invalid
```

`IpcPort` 创建后先处于 `Invalid`，发布成功后进入 `Listening`。
关闭 port 会从全局 registry 中移除自身，并关闭所有 pending server endpoint。

### Channel 生命周期

```text
client new_client()
  ▼
Connecting
  ├─ attach_client() creates server endpoint
  │      server: Accepting
  │
  ├─ server complete_accept()
  ▼
Connected
  ├─ ipc_send_msg / ipc_get_msg / ipc_read_msg / ipc_put_msg
  └─ close() or peer close
      ▼
Disconnecting
```

client endpoint 可以先于目标 port 创建。
当 port 已发布或稍后发布时，`attach_client` 才分配双方接收队列并创建 server endpoint。

### Message slot 状态

```text
Free
  └─ push()
      ▼
Filled
  └─ peek_next_filled() + successful get_filled()
      ▼
Read
  ├─ read()
  ├─ read_handles()
  └─ put()
      ▼
Free
```

`read` 不释放 slot，只有 `put` 会清空数据和 attached handles，并把 slot 放回 free list。

### Handle table 生命周期

```text
Arc<dyn Handle>
  └─ uctx_handle_install()
      ▼
process-local integer id
  ├─ uctx_handle_get()
  ├─ wait_any_snapshot()
  ├─ register_wait_any_table_change()
  └─ uctx_handle_remove()
       ├─ detach_from_handle_sets(id)
       └─ handle.close()
```

handle id 是进程本地能力，不是全局 id。
关闭一个 handle 时，`HandleTable` 会从同一 table 中所有 `HandleSet` 移除该 id，避免 stale registration 持有已关闭对象。
`sys_tipc_wait_any` 使用 `HandleTable` 缓存的 wait-any snapshot；
缓存只在 handle table 增删后重建，避免每轮 poll 都分配 `Vec` 并 clone
所有 `Arc<dyn Handle>`。等待路径同时注册 handle 事件和 handle-table
membership 变化，防止安装或关闭 handle 时漏唤醒。

## 算法流程

### 创建并发布 port

```text
ipc_port_create(uuid, path, queue sizes, flags)
  → validate_port_path()
  → IpcPort::new()
  → caller keeps Arc<IpcPort>

ipc_port_publish(port)
  → registry lock
  → remove stale weak port for same path
  → reject duplicate live port
  → mark_published()
  → insert Weak<IpcPort>
  → remove waiting clients for path
  → unlock registry
  → attach each live waiting client
```

waiting client 在 registry 解锁后逐个 attach，避免在全局 registry 锁内执行较复杂的 channel 创建和 port pending-list 操作。
`IpcPort` 内部的 port-local 临界区会重新检查 `Listening` 状态，并把 client/server 绑定与 pending 队列更新串行化，因此并发 close 不能插入到状态检查和 pending 入队之间。

### 连接 port

```text
ipc_port_connect_async(uuid, path, flags)
  → validate flags and path
  → create client endpoint in Connecting
  → registry lookup path
      ├─ live port exists: attach immediately
      ├─ missing + WAIT_FOR_PORT: store Weak<IpcChan>
      └─ missing: NotFound
  → return client endpoint
```

同步连接由 syscall adapter 或调用方在返回 client endpoint 后调用 `wait_connected` 完成。

### Accept

```text
IpcPort::ipc_port_accept()
  → pending_list.pop_front()
  → server.complete_accept()
      ├─ server Accepting -> Connected
      ├─ peer Connecting -> Connected
      └─ notify client READY
  → return (server endpoint, peer uuid)
```

### 发送和接收消息

```text
sender.ipc_send_msg_with_handles(data, handles)
  → check local and peer are Connected
  → peer.msg_queue.push(data, handles)
  → notify peer MSG
  → if queue full, mark peer_send_blocked

syscall::send_msg(user iovecs)
  → copy at most peer receive slot size
  → sender.ipc_send_msg_with_handles(copied data, handles)

receiver.ipc_get_msg()
  → peek_next_filled metadata
  → after metadata reaches userspace, get_filled moves filled slot -> read slot

receiver.ipc_read_msg(id, offset, out)
  → copy bytes from read slot

syscall::read_msg(user iovecs)
  → under one message-queue lock, copy requested bytes and clone attached handles
  → after releasing the queue lock, copy bytes to user iovecs and install handles

receiver.ipc_put_msg(id)
  → read slot -> free slot
  → if queue became writable, notify peer SEND_UNBLOCKED
```

### Handle set poll

```text
handle_set_ctrl(Add/Modify/Delete...)
  → update BTreeMap<handle_id, HandleSetEntry>
  → notify handle set waiters

poll_one()
  → scan entries in key order
  → poll underlying handle with finalize=true
  → mask with requested event bits
  → return first ready UEvent
```

当前实现禁止把 `HandleSet` 添加到另一个 `HandleSet`。
这比 Trusty 的“允许非循环嵌套”更保守，避免第一阶段引入循环检测。

## 并发模型

- `PortRegistry` 使用全局 `SpinNoIrq<PortRegistryState>` 保护 path registry 和 waiting client 列表。
- `IpcPort::inner` 使用 `SpinNoIrq` 同时保护 port lifecycle state 和 pending server endpoint 队列，避免 attach/accept/close 之间拆开状态检查与队列更新。
- `IpcChan::peer` 和 `msg_queue` 使用 `SpinNoIrq`，生命周期状态和辅助事件使用 atomic。
- `IpcMsgQueue` 不自带锁，只在 `IpcChan::msg_queue` 锁内访问。
- `HandleSet::entries` 使用 `SpinNoIrq<BTreeMap<...>>`，注册、删除和 poll 串行化。
- `HandleWaitState` 使用 `kpoll::PollSet` 保存等待者，并用 `AtomicUsize` 保存 cookie。
- `HandleTable` 不内置锁，进程状态通常以 `RwLock<HandleTable>` 包装。

锁粒度偏向简单和局部化。
全局 registry 锁只保护命名空间，channel 和 message 操作不持有 registry 锁。

## 设计决策

- **syscall adapter 跟随 TIPC owner**：Trusty IPC syscall ABI 放在 `tipc` crate 中，便于把 TIPC 专属 ABI、UUID 转换和核心状态机一起维护；`core/ksyscall` 只保留总分发表。
- **handle ownership 独立为 `tipc-handle`**：`process/kprocess` 只需要 process-local handle table，不应依赖完整 TIPC core；`tipc` 与 `kprocess` 共同依赖 `tipc-handle` 来避免循环依赖。
- **registry 保存 weak port**：port 生命周期由创建者的 handle table 强引用拥有，全局命名空间不阻止对象释放。
- **client 可等待未发布 port**：`WAIT_FOR_PORT` 通过保存 weak client endpoint 实现，若 client 在 port 发布前关闭，发布时会跳过 stale weak。
- **固定槽消息队列**：保留 Trusty message boundary 和 back-pressure 语义，队列满时返回 `WouldBlock` 并在释放 slot 后通知发送方。
- **handle set 禁止嵌套**：第一阶段避免循环检测复杂度；后续如需对齐 Trusty，可扩展为非循环嵌套。
- **memref 暂为元数据 handle**：`sys_memref_create` 验证页对齐的地址范围被当前进程 VMA 覆盖，并具有请求的 READ/WRITE 用户权限；handle 当前保存通过校验的地址、长度和权限位，不建立跨地址空间映射。
- **memref mmap 语义先校验后接入**：`MemRef::validate_mmap` 按 Trusty 规则校验 offset、size 和请求权限是否落在 memref 能力内；实际 VMM 对象绑定和目标地址空间映射仍待接入。

## 内核调用方

TIPC core 的 port、channel、handle set 对象同时服务 syscall adapter 和内核内部调用方。
`knet` 的 vsock-TIPC bridge 不经过进程 handle table，而是直接复用下列对象 API：

- `ipc_port_connect_async(..., WAIT_FOR_PORT | ASYNC)` 创建 host-originated client channel。
- `ipc_port_create` + `ipc_port_publish` 创建 bridge 暴露给 TA 的 forwarder service。
- `IpcPort::ipc_port_accept` 非阻塞接收 TA-originated channel。
- `IpcChan::ipc_send_msg`、`ipc_get_msg`、`ipc_read_msg`、`ipc_put_msg` 转发 message bytes 并保持 Trusty message boundary。
- `HandleSet::handle_set_create`、`handle_set_ctrl`、`poll_one` 让 bridge worker 等待 READY、MSG、SEND_UNBLOCKED、HUP 和 ERROR。

bridge v1 只转发消息字节。
如果 TIPC message 附带 handle 或 memref，bridge 会把该消息视为不支持并关闭连接；
跨 vsock 转发 object capability 需要单独的能力模型和生命周期设计。

## Drop / 资源释放

- `IpcPort::drop` 调用 `close`，从 registry 移除自身并关闭 pending channel。
- `IpcChan::drop` 调用 `close`，进入 `Disconnecting`，关闭本端接收队列，通知对端 HUP/ERROR。
- `HandleTable::uctx_handle_remove` 会先从所有 handle set 移除目标 id，再调用目标 handle 的 `close`。
- `IpcMsgQueue::put` 清空 slot 数据和 attached handles，释放对 transferred handles 的强引用。
- `MemRef::close` 当前只唤醒等待者；实际 VMM 资源解绑尚未实现。
