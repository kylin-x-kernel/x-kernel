# knet — 安全与可靠性分析

## 信任模型

```text
posix/net syscall 层
   │
   │ safe API: SocketOps, Socket, SocketAddrEx, options
   v
┌─────────────────────────────┐
│ knet                        │
│                             │
│ safe boundary               │
│  ├─ TCP / UDP / raw socket  │
│  ├─ Unix socket             │
│  ├─ netlink socket          │
│  └─ Service / Router        │
│                             │
│ unsafe boundary             │
│  ├─ TcpSocket Sync impl     │
│  ├─ Unix stream ring buffer │
│  └─ vsock RX ring advance   │
└──────────────┬──────────────┘
               │
               v
driver layer / smoltcp / ringbuf
```

- safe API 调用者信任 `knet` 维护 socket 状态、路由状态、buffer 边界和 errno 映射。
- `knet` 信任 `posix/net` 完成用户指针读取、sockaddr 长度校验、地址族选择和权限策略。
- `knet` 信任 driver 层返回的 `NetBufHandle` 数据切片在 handle 生命周期内有效。
- `knet` 信任 smoltcp checked parser 拒绝格式错误的 IP、TCP、UDP、ARP 和 Ethernet 数据。
- unsafe 边界由 crate 内部封装，外部调用者没有直接调用 unsafe API 的入口。

## 外部边界 / 攻击面

`knet` 是面向外部输入最丰富的 crate 之一，
攻击面不仅来自 Rust `unsafe`，
还来自网络包、控制面消息、设备缓冲区和上层 syscall glue。

经检查，本模块直接或间接接触以下边界：

- **网络输入**：Ethernet、ARP、IPv4、TCP、UDP、raw IP 包；
- **设备输入**：driver 提供的 `NetBufHandle`、IRQ 唤醒、RX/TX 缓冲区生命周期；
- **控制面输入**：netlink header、attribute、route mutation、neighbor 更新；
- **Unix / vsock 输入**：peer socket 数据、连接建立与关闭事件；
- **上层 syscall 语义输入**：由 `posix/net` 传入的 socket 地址族、
  socket option、阻塞语义和权限决策结果。

本模块不直接解引用用户指针，
而是信任 `posix/net` 先完成用户内存访问、长度校验和权限检查。
本模块也不直接执行 DMA 编程，
但会消费由 driver 层提供的网络缓冲区，
因此仍需把 driver buffer 生命周期视为关键边界。

因此威胁分析重点应覆盖：

- 外部网络包是否能触发越界、panic、状态不一致或资源耗尽；
- 控制面 mutation 是否会让 `ROUTE_STATE`
  与 data-plane 同步失配；
- driver buffer、socket handle、ring buffer
  是否可能因生命周期管理错误破坏内存安全；
- IRQ 路径与普通协议推进路径是否可能发生竞态或延迟放大。

## unsafe 代码清单

### 1. `TcpSocket` 的 `Sync` 实现

位置：`src/transport/tcp.rs:60`

```rust
unsafe impl Sync for TcpSocket {}
```

不变量：

- `TcpSocket` 中可变共享状态必须由 `StateLock`、`Mutex`、`RwLock` 或 atomic 保护。
- smoltcp socket 只能通过 `SOCKET_SET.with_socket_mut` 或 `SOCKET_SET.inner.lock()` 访问。
- `dispatch_irq` 必须引用仍在 `SOCKET_SET` 中的 socket handle。
- `accepted_remote_endpoint` 构造后只读。

安全依据：

- `state` 使用 `AtomicU8` 和 CAS 管理状态转换。
- `bound_endpoint` 使用 `Mutex`。
- shutdown 标志使用 `AtomicBool`。
- smoltcp socket set 使用全局 `Mutex<SocketSet>` 串行访问。
- accepted child socket 的 endpoint 在构造后没有修改入口。

调用者：

- `posix/net::sys_socket` 创建 `TcpSocket` 后通过 `Socket` 文件对象共享。
- poll、send、recv、accept 路径通过 `SocketOps` safe trait 访问。

### 2. Unix stream 写入 vacant ring buffer

位置：`src/unix/stream.rs:255` 和 `src/unix/stream.rs:259`

```rust
let mut count = src.read(unsafe { left.assume_init_mut() })?;
count += src.read(unsafe { right.assume_init_mut() })?;
```

不变量：

- `left` 和 `right` 来自同一次 `HeapProd::vacant_slices_mut`。
- 两个切片表示当前 producer 的可写容量。
- `Read::read` 只初始化它返回的字节数。
- channel lock 持有期间，其他路径无法同时推进同一 producer。

安全依据：

- `send` 持有 `self.channel.lock()` 后取得 `chan.tx`。
- 写入后只发布 `Read::read` 返回的字节数。
- `ringbuf` producer 自身维护环形 buffer 边界。

调用者：

- `StreamTransport::send`，由 Unix stream socket `SocketOps::send` 调用。

### 3. Unix stream 推进 write index

位置：`src/unix/stream.rs:264`

```rust
unsafe { chan.tx.advance_write_index(count) };
```

不变量：

- `count` 等于本次写入 `left` 与 `right` 的总字节数。
- `count` 小于等于本次 `vacant_slices_mut` 暴露的总容量。
- 推进 write index 前，相关字节已经初始化。

安全依据：

- `count` 只由两个 `Read::read` 返回值累加得到。
- 每次 `Read::read` 的目标切片长度受 ring buffer vacant 区域限制。
- channel lock 保护 producer。

调用者：

- `StreamTransport::send`。

### 4. Unix stream 推进 read index

位置：`src/unix/stream.rs:296`

```rust
unsafe { chan.rx.advance_read_index(count) };
```

不变量：

- `count` 等于本次从 `chan.rx.as_slices` 返回的 occupied 区域复制出的字节数。
- `count` 小于等于当前 readable 区域长度。
- PEEK 语义没有进入此路径。

安全依据：

- `recv` 持有 `self.channel.lock()` 后访问 `chan.rx`。
- `dst.write` 的输入切片来自 `as_slices` occupied 区域。
- `count` 由两个 `dst.write` 返回值累加得到。

调用者：

- `StreamTransport::recv`。

### 5. vsock 推进 RX read index

位置：`src/vsock/connection_manager.rs:161`

```rust
unsafe {
    self.rx_consumer.advance_read_index(count);
}
```

不变量：

- 调用者持有 connection lock。
- `count` 来自同一 connection 的 `rx_slices` 复制结果。
- `count` 小于等于当前 RX occupied 长度。

安全依据：

- `VsockStreamTransport::recv` 在 `conn.lock()` 保护下读取 `rx_slices` 并调用 `advance_rx_read`。
- PEEK 请求跳过 advance。
- ringbuf consumer 只在 connection lock 内推进。

调用者：

- feature `vsock` 下的 `VsockStreamTransport::recv`。

## 内存安全不变量

1. `SocketHandle` 生命周期：任何 smoltcp socket handle 在访问前必须仍存在于 `SOCKET_SET`。
2. `SocketSet` 互斥访问：所有 `SocketSet` 读写必须经过 `SOCKET_SET.inner` 的 mutex。
3. `Service` 互斥访问：smoltcp `Interface`、`Router` 和设备 dispatch 必须在 `SERVICE.lock()` 内推进。
4. ring buffer publish 规则：Unix stream 和 vsock 只能发布已经写入的字节，只能消费当前 occupied 区域内的字节。
5. driver buffer 生命周期：`NetBufHandle::data` 只在 handle 被 recycle 前使用，RX handle 必须在处理后归还。
6. netlink message 边界：所有 payload 读取必须先经过 header 长度、attribute 长度和 family 校验。
7. route index 边界：rtnetlink route 的 `oif` 转换成设备索引后必须检查 `dev < devices.len()`。
8. static init 顺序：创建 socket 前必须完成 `init_network` 初始化 `SERVICE`、`SOCKET_SET` 和 `LISTEN_TABLE`。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `TcpSocket` | 字段满足 Send | unsafe impl，依赖内部锁和 global socket set 串行化 |
| `UdpSocket` | 字段满足 Send | `RwLock`、atomic 和 immutable handle 保护共享状态 |
| `RawSocket` | 字段满足 Send | `RwLock`、atomic 和 immutable handle 保护共享状态 |
| `StreamTransport` | 字段满足 Send | `Mutex<Option<Channel>>`、atomic 和 `PollSet` 保护共享状态 |
| `NetlinkSocket` | 字段满足 Send | `Arc<NetlinkSocketInner>` 内部使用 `RwLock`、`Mutex` 和 `PollSet` |
| `Service` | 在 `Mutex<Service>` 内使用 | 通过全局 `Mutex` 提供共享访问 |
| `Router` | 在 `Service` 内使用 | 通过 `Service` mutex 间接共享 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 初始化顺序错误导致 `LazyInit` 访问未初始化对象 | 高 | socket 在 `init_network` 前创建或 poll | 启动路径由 `core/kruntime` 先调用 `init_network`；新增入口需保留该顺序 |
| T-02 | smoltcp socket handle 已删除后继续访问 | 高 | listener 关闭、accepted child 清理与并发 socket 操作交错 | `SocketSet` 访问由 mutex 串行化；`ListenTable::unlisten` drain 后删除 handle；调用点需避免缓存 handle 后跨释放点使用 |
| T-03 | ring buffer index 推进超过实际写入或读取长度 | 高 | `advance_write_index` 或 `advance_read_index` 的 count 与切片来源不一致 | count 由同一锁内的 read/write 返回值计算；unsafe 注释固定不变量 |
| T-04 | 恶意 netlink 消息越界读取或构造非法状态 | 高 | 用户传入短 header、畸形 attr、非法 family、非法 ifindex | `NlMsgHeader::read`、`parse_attrs`、`parse_ip_by_family` 和 route index 检查拒绝非法输入 |
| T-05 | ARP spoofing 污染 neighbor cache | 中 | 外部主机发送伪造 ARP reply 或 request | 当前校验 unicast MAC、广播和本机目标 IP；完整邻居安全策略仍依赖网络隔离 |
| T-06 | listen backlog 被 SYN 洪泛占满 | 中 | 大量连接请求命中同一 listener | backlog 被 clamp 到 `LISTEN_QUEUE_SIZE`；超限丢弃并记录 warn |
| T-07 | raw socket 被无权限调用者创建 | 中 | syscall 层没有实施权限门禁 | `knet` 层只封装 raw socket 行为；权限策略应保留在 `posix/net::sys_socket` |
| T-08 | netlink RX queue 被 uevent 或 response 填满 | 中 | 订阅者不消费，publisher 持续写入 | `NETLINK_RX_QUEUE_LIMIT` 限制单 socket queue 字节数，超限丢弃 |
| T-09 | 路由状态与 data-plane 不一致 | 中 | rtnetlink mutation 只更新控制面或同步过程中出错 | `update_route_state` 写入 `ROUTE_STATE` 后调用 `SERVICE.lock().sync_netlink`；新增 mutation 需复用该路径 |
| T-10 | 外部网络包触发 parser panic | 中 | malformed Ethernet、ARP、IP 或 TCP packet 进入 RX | RX 路径使用 smoltcp checked parser，错误包丢弃并记录 warn |
| T-11 | 中断上下文误用导致锁竞争或延迟放大 | 中 | IRQ waker 回调中直接推进 `SERVICE`、`SOCKET_SET` 或执行阻塞 socket 操作 | 中断路径只注册和唤醒 waker，协议推进保留在普通 poll 路径 |
| T-12 | driver buffer 或 DMA 输入破坏 packet 边界 | 高 | 驱动返回长度异常、数据在 recycle 后继续被访问、TX/RX buffer 生命周期使用错误 | RX 数据只在 `NetBufHandle` recycle 前解析和复制；外部帧使用 checked parser；TX buffer 由 driver handle 管理 |
| T-13 | vsock-TIPC bridge 误把普通 AF_VSOCK 连接路由到 TIPC | 中 | 事件分流没有区分桥接端口或已桥接连接 | bridge 只接管静态 port map 和自己的 connection id，未命中事件继续交给 `VSOCK_CONN_MANAGER` |
| T-14 | host 通过 bridge 注入超大或非法 TIPC message | 中 | `Received` record 超过 TIPC slot 或 port 0 service name 非法 | bridge 限制 record 长度为 `IPC_CHAN_MAX_BUF_SIZE`，动态 service name 需通过 UTF-8、NUL 和长度校验；非法 name 回 `[1]` 并断开 |
| T-15 | TIPC handle/memref capability 经 vsock 泄露到 host | 高 | TA 向 bridge 发送带 attached handles 的 message | bridge v1 只转发 bytes，发现 attached handles 时关闭连接 |
| T-16 | host 误判 port 0 handshake 结果 | 中 | 未读状态字节就发 payload；忽略 `[1]`；无 recv 超时导致永久阻塞 | 协议要求 host 先读单字节状态（`0`=成功，`1`=拒绝）；`libtrusty` 使用 `SO_RCVTIMEO`；CA 测试拒绝非 `[0]` 状态 |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 网络设备缺失 | `DeviceContainer<NetDevice>` 为空 | 只有 loopback 可用 | 对外 TCP、UDP、raw IP 返回无路由或连接失败 | 3 | `init_network` 记录 warn，`Service::get_source_address` 返回 `ENETUNREACH` |
| F-02 | 目的地址无路由 | `RouteTable::lookup` 未命中 | 当前发送失败 | 应用连接失败 | 3 | connect 或 send 路径返回 `ENETUNREACH`，dispatch 路径 warn 后丢弃 |
| F-03 | Ethernet pending queue 满 | ARP 未解析且 `pending_tx` 达到上限 | 后续 IP 包丢弃 | 单目的或多目的通信丢包 | 3 | 记录 warn；后续需按 next-hop 拆分队列降低 head-of-line blocking |
| F-04 | driver TX buffer 分配失败 | NIC driver 返回 `alloc_tx_buf` 错误 | 当前 frame 未发送 | 网络吞吐下降或连接超时 | 3 | 记录 warn 并返回；上层 poller 可继续重试 |
| F-05 | UDP ICMP error queue 丢失 | error registry 未注册或 socket 已关闭 | `SO_ERROR` 或 error queue 缺失 | 应用无法获得异步网络错误 | 4 | bind 时注册 `UdpErrorState`，Drop 路径需保持 unregister |
| F-06 | TCP accepted child abort | 握手期间 peer reset 或 smoltcp child 关闭 | `accept` 返回 `ConnectionAborted` | 应用重试 accept | 4 | `ListenTable::accept` 清理 closed child 并继续扫描队列 |
| F-07 | poll waker 丢失 | device mask 错误或 timeout 未注册 | socket 阻塞等待延迟 | 应用 IO latency 上升 | 3 | bind/connect 后更新 device mask；`Service::register_rx_waker` 同时注册 timeout poll |
| F-08 | malformed netlink request | header 或 attr 长度非法 | 返回 empty response 或 netlink error | 调用者请求失败 | 4 | checked reader 和 error response 处理 |
| F-09 | ROUTE_STATE 未初始化 | netlink route 请求早于 `init_route_state` | panic 或空状态 | netlink 功能不可用 | 2 | 初始化路径在 `init_network` 中创建初始 state；新增启动路径需保持顺序 |
| F-10 | Unix stream peer 提前关闭 | channel 被 shutdown 或 drop | send 返回 `BrokenPipe`，recv 返回 EOF | 应用感知连接关闭 | 4 | shutdown 设置 atomic 并唤醒 peer poll set |
| F-11 | 中断上下文执行重型网络推进 | IRQ 路径误调用 socket send、recv 或 `poll_interfaces` | 锁竞争、调度延迟或死锁 | 网络 IO 延迟上升，严重时系统卡顿 | 2 | IRQ 路径只做 waker notification，实际协议推进由 task 上下文执行 |
| F-12 | RX buffer recycle 顺序错误 | frame payload 在 `recycle_rx` 后仍被引用 | 读取悬垂数据或数据损坏 | packet 解析异常，严重时破坏内存安全 | 1 | `EthernetDevice::poll_rx` 在 recycle 前完成解析和复制；新增设备适配需保持同样生命周期 |
| F-13 | port 0 handshake 永久等待 | host 早于 TA publish 连接且未设 recv 超时；或 service 永不 publish | host `read` 阻塞；负例测试挂起 | CA/测试进程无响应 | 3 | dynamic connect 保留 `WAIT_FOR_PORT`；host 设 `TRUSTY_VSOCK_TIMEOUT_SEC`；明确拒绝场景回 `[1]` |
| F-14 | 快速重连 `tipc_connect` 超时 `-11` | `route_event` 在 `has_connection()` 前丢弃同批 `Received`，service-name record 丢失 | host status-byte `EAGAIN`；约半数快速重连失败 | storage client/proxy harness 间歇失败 | 2 | mapped bridge port 仅按 `local_port` 认领；事件入 FIFO，不依赖 `has_connection()` |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- 普通输入错误使用 `KError` 和 `LinuxError` 返回，例如 `EINVAL`、`EAFNOSUPPORT`、`ENETUNREACH`、`EADDRINUSE`、`EWOULDBLOCK`。
- malformed netlink 请求返回 netlink error response，短到无法读取 header 的请求返回空 response。
- malformed Ethernet、ARP、IP、TCP 包在 RX 路径丢弃，并通过 warn 或 trace 记录。
- smoltcp buffer full 映射为 `WouldBlock`，poller 负责等待 IO readiness。
- loopback 和 Ethernet 队列满时丢包并记录 warn。
- panic 路径主要来自初始化顺序、内部 invariant 破坏和 `expect` 断言；新增公开入口应先返回 `KError`，再进入内部断言区。

## 隐私分析

`knet` 会处理用户进程通过 socket 发送的 payload、从网络收到的 packet payload、Unix socket credentials、netlink 消息和 vsock payload。
这些数据在内核内按 socket buffer、ring buffer、driver buffer 或 netlink queue 保存。
模块自身不做持久化，也不把 payload 写入日志；trace 日志当前会输出 Ethernet frame 字节，生产环境需按日志级别控制敏感网络数据泄露。

## 已知限制

- `RTM_GETNEIGH` dump 尚未实现，neighbor 只通过 `RTM_NEWNEIGH` mutation 进入控制面。
- `ROUTE_STATE` dump 没有覆盖所有 live runtime 状态。
- raw socket 创建权限由 syscall 层承担，`knet` 构造器自身没有进程凭据参数。
- Ethernet 设备只处理 IPv4 ARP，IPv6 NDP、非 Ethernet 链路和多队列 NIC 抽象仍待扩展。
- vsock-TIPC bridge v1 不转发 TIPC handles 或 memrefs，也不为 vsock send credit 建立持久重试队列。

## 审计清单

修改本模块时需验证：

- 每个 `unsafe` 块均有 `SAFETY:` 注释。
- 新增 smoltcp socket handle 的生命周期受 `SOCKET_SET` 保护。
- 新增 route 或 device mutation 通过 `update_route_state` 或等效同步路径更新 data-plane。
- 新增 netlink parser 先校验 header 长度、attribute 长度、family 和 index。
- 新增 ring buffer 操作的 advance count 来自同一锁内同一批 slices。
- 新增外部网络输入使用 checked parser。
- 新增 socket option 明确 errno、阻塞语义和 poll readiness。
- 新增公开 API 先确认是否需要跨 crate 暴露。
