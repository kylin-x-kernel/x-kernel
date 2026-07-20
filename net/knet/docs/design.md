# knet — 设计文档

## 定位

`knet` 提供 x-kernel 的内核网络栈抽象。
它把驱动层网卡、smoltcp 协议栈、socket 对象、轮询唤醒、Unix domain socket、netlink 和可选 vsock 组合成统一的 `SocketOps` 接口。
上层 `posix/net` 负责 syscall 参数解析和文件描述符接入，`core/kruntime` 负责启动期设备注入，`knet` 负责协议状态、路由、收发推进和 socket 语义。

## 背景

x-kernel 运行在 `no_std` 内核环境中，无法直接使用 Linux 内核网络栈或标准库网络类型。
当前实现以 smoltcp 作为 TCP、UDP、raw IP 的协议引擎，并在 crate 内补齐内核需要的设备适配、监听表、poller、socket option、rtnetlink 控制面、Unix socket 和 vsock glue。

## 范围

涉及的源文件：

```text
net/knet/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── device/
    │   ├── mod.rs
    │   ├── ethernet.rs
    │   ├── loopback.rs
    │   └── vsock.rs
    ├── link/
    │   ├── buf.rs
    │   ├── mod.rs
    │   ├── packet.rs
    │   └── wire.rs
    ├── netlink/
    │   ├── mod.rs
    │   ├── route.rs
    │   ├── socket.rs
    │   └── wire.rs
    ├── socket/
    │   ├── mod.rs
    │   ├── file.rs
    │   ├── general.rs
    │   ├── options.rs
    │   └── state.rs
    ├── stack/
    │   ├── ipv4.rs
    │   ├── mod.rs
    │   ├── service.rs
    │   ├── router.rs
    │   ├── listen_table.rs
    │   └── wrapper.rs
    ├── transport/
    │   ├── mod.rs
    │   ├── tcp.rs
    │   ├── udp.rs
    │   ├── raw.rs
    │   └── udp_err.rs
    ├── unix/
    │   ├── dgram.rs
    │   └── stream.rs
    └── vsock/
        ├── connection_manager.rs
        └── stream.rs
```

测试辅助代码位于 `socket/test_options.rs`、`socket/test_state.rs` 和 `netlink/tests.rs`。

## 架构

```text
core/kruntime
   │ init_network
   v
┌──────────────────────────┐
│ Service                  │
│  ├─ smoltcp Interface    │
│  ├─ Router               │
│  └─ timeout PollSet      │
└──────────┬───────────────┘
           │
           v
┌──────────────────────────┐
│ Router                   │
│  ├─ RouteTable           │
│  ├─ LoopbackDevice       │
│  └─ EthernetDevice       │
└──────────┬───────────────┘
           │
           v
┌──────────────────────────┐
│ SocketSetWrapper         │
│  └─ smoltcp SocketSet    │
└──────────┬───────────────┘
           │
           v
┌──────────────────────────┐
│ SocketOps implementations│
│ TCP / UDP / raw / Unix   │
│ netlink / vsock          │
└──────────────────────────┘
```

| 组件 | 职责 |
|------|------|
| `init_network` | 创建 loopback 与首个 Ethernet 设备，建立默认路由，初始化 `Service`、`SocketSetWrapper`、`ListenTable` 和 rtnetlink 初始状态 |
| `Service` | 持有 smoltcp `Interface`、`Router`、poll timeout 和 RX waker 注册入口 |
| `Router` | 管理设备列表、路由表、`PacketBuf` RX/TX 队列、IPv4 输入校验、next-hop 选择和设备 dispatch |
| `NetDevice` | 抽象 loopback、Ethernet 和 feature gated vsock 设备后端，通过 `PacketBuf` 转移报文所有权，并使用 crate 内地址类型和 `TimeValue` 表达设备边界 |
| `PacketBuf` | 保存报文数据、协议偏移、接口索引、包类型、校验状态和当前所有者 |
| `link::wire` | 使用 `zerocopy` 校验和构造 Ethernet 与 Ethernet/IPv4 ARP 头部 |
| `stack::ipv4` | 使用 `etherparse` 校验和构造 IPv4 与 ICMPv4 头部，并提供最终版分片相关 API |
| `SocketSetWrapper` | 串行化 smoltcp socket set 访问，并在新增 socket 时通知等待者 |
| `ListenTable` | 管理 TCP listen backlog、SYN 队列、accept 队列和 accept waker |
| `GeneralOptions` | 统一管理 nonblock、reuseaddr、超时和设备 mask |
| `SocketOps` | 上层 socket syscall 使用的统一操作接口 |
| `netlink` | 提供 AF_NETLINK socket、kobject uevent 和有限 rtnetlink 控制面 |
| `unix` | 提供 Unix domain stream 与 datagram transport |
| `vsock` | 在 `vsock` feature 下提供 virtio-vsock stream 支持 |

## 调用约束 / 执行上下文

`knet` 运行在内核上下文中，
但并非所有路径都适用于任意执行环境。
调用者需要满足以下约束：

- **依赖初始化顺序**：在创建 socket、访问路由状态、
  或推进协议栈之前，必须先完成 `init_network`，
  以初始化 `SERVICE`、`SOCKET_SET`、`LISTEN_TABLE`
  和 netlink 初始状态。
- **普通协议推进不应在硬中断上下文执行**：
  `poll_interfaces`、socket send/recv/connect/accept、
  netlink mutation 等路径会获取 `Mutex` / `RwLock`
  并推进较重的数据路径，应在普通任务上下文中运行。
- **IRQ 路径只做通知**：
  设备中断回调应只负责唤醒等待者或登记事件，
  不应直接执行完整的协议推进或阻塞式 socket 语义。
- **允许阻塞的路径依赖 poll/waker 语义**：
  阻塞式 socket 操作依赖 `PollSet`、waker
  和 timeout 注册机制，调用者必须保证相应调度/等待环境可用。
- **不要求固定当前进程线程才能访问全局状态**：
  `SERVICE`、`SOCKET_SET` 和 `ROUTE_STATE`
  的共享访问主要依赖全局锁和原子状态，
  但 syscall 语义相关路径仍由 `posix/net`
  负责提供进程文件描述符与凭据语境。
- **可重入性受全局锁约束**：
  允许多执行路径并发进入 crate，
  但同一时刻对 `Service`、`SocketSet`
  或 listener backlog 的关键访问会被串行化。

## 状态机

### TCP socket 状态

`TcpSocket` 使用 `StateLock` 记录用户可见状态，并通过 smoltcp socket 状态完成协议推进。

```text
Idle ──bind/connect/listen──> Busy
Busy ──connect pending──────> Connecting
Busy ──listen ok───────────> Listening
Connecting ──smoltcp established──> Connected
Listening ──accept child──────────> Listening
Connected ──shutdown/close────────> Closed
Busy ──operation error────────────> previous state
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| `Idle` | `Busy` | `StateLock::lock` 赢得状态转换 |
| `Busy` | `Connecting` | `connect` 成功提交到 smoltcp |
| `Busy` | `Listening` | `listen` 注册到 `ListenTable` |
| `Connecting` | `Connected` | `poll_connect` 观察到 smoltcp `Established` |
| `Connected` | `Closed` | shutdown 或底层状态进入关闭路径 |
| `Busy` | 原状态 | `StateGuard::transit` 内部操作返回错误 |

### rtnetlink 控制面状态

```text
build_initial_state
        │
        v
ROUTE_STATE
        │ RTM_NEWLINK / RTM_NEWADDR / RTM_NEWROUTE / RTM_NEWNEIGH
        v
updated ROUTE_STATE ──sync_netlink──> Service / Router / Interface / devices
        │
        └─RTM_GETLINK / RTM_GETADDR / RTM_GETROUTE──> netlink dump
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| 初始 state | `ROUTE_STATE` | `init_network` 调用 `build_initial_state` 与 `init_route_state` |
| `ROUTE_STATE` | 更新后的 `ROUTE_STATE` | `apply_newlink`、`apply_newaddr`、`apply_newroute`、`apply_newneigh` |
| 更新后的 `ROUTE_STATE` | data-plane 同步 | `update_route_state` 调用 `SERVICE.lock().sync_netlink` |

## 算法流程

### 网络初始化

1. `init_network` 创建 `Router`。
2. 添加 `LoopbackDevice`，注册 `127.0.0.1/8` 路由。
3. 从 `DeviceContainer<NetDevice>` 取首个 NIC，包装成 `EthernetDevice`，注册默认 IPv4 路由。
4. 创建 `Service`，把 loopback 和 Ethernet 地址写入 smoltcp `Interface`。
5. 初始化 rtnetlink 初始状态，并同步到 `Service`、`Router` 和设备。
6. 初始化全局 `SOCKET_SET`、`LISTEN_TABLE` 和 UDP 异步错误 registry。

### RX 推进

1. `poll_interfaces` 获取 `SERVICE` 和 `SOCKET_SET` 锁。
2. `Service::poll` 调用 `Router::poll`，设备以 `PacketBuf` 形式逐包转移 RX 所有权。
3. `Router::poll` 根据网络层版本分流，IPv4 包先校验头部、长度、校验和和本地目的地址，再执行 UDP error snoop 和 TCP listen snoop。
4. 校验后的 IPv4 或 IPv6 数据复制到 smoltcp RX adapter，`Interface::poll` 驱动现有协议 socket。
5. `ListenTable::wake_touched_acceptors` 唤醒 accept 等待者。
6. `Router::dispatch` 把 smoltcp 生成的 TX IP 包按路由发到设备。
7. loopback 或设备 TX 触发后继续返回 `true`，外层循环继续推进。

### TX 路由

1. smoltcp 把待发 IP 包写入 `Router` 的 `PacketBuf` TX 队列。
2. `Router::dispatch` 接管所有权，并按 IP 版本解析目的地址；IPv4 输出先按实际长度更新 `total_len` 和头部校验和。
3. 广播或组播包复制到所有设备。
4. 单播包通过 `RouteTable::lookup` 选择最长前缀路由。
5. Ethernet 设备先查 ARP neighbor cache，命中后通过 `link::wire` 封装 Ethernet frame。
6. 未命中时发送 ARP request，并把 IP 包放入 `pending_tx` 等待 neighbor 解析。

当前 PR 保留 smoltcp TCP、UDP、raw socket 和 IPv6 推进。`PacketBuf`、`link::wire`
和 `stack::ipv4` 使用最终接口，后续协议迁移直接复用这些类型和 parser/emitter。
路由表和设备接口使用 `crate::ip` 地址类型，设备时间使用 `khal::time::TimeValue`。
`Router`、`Service` 和初始化入口在 smoltcp 兼容边界完成地址与时间转换。

### TCP listen 和 accept

1. `TcpSocket::listen` 把本地 endpoint 注册到 `ListenTable`。
2. `Router::poll` snoop 首个 SYN 包，并调用 `ListenTable::incoming_tcp_packet`。
3. `ListenTable` 为新连接创建一个 smoltcp TCP socket，放入 SYN 队列。
4. 后续 poll 观察到连接可接受后移动到 accept 队列。
5. `TcpSocket::accept` 取出 `AcceptedTcp`，构造新的 connected `TcpSocket`。
6. POSIX 层通过 `AcceptOptions` 传入监听文件的 nonblocking 状态；accept 队列为空时，
   nonblocking 调用返回 `WouldBlock`，由用户态 poll/epoll 负责等待下一次可读事件。

### rtnetlink 请求

1. `NetlinkSocket::send` 把用户请求交给 `handle_route_request`。
2. `NlMsgHeader::read` 和各类 `parse_*` helper 校验 netlink header 与 attribute。
3. dump 请求从 `ROUTE_STATE` 生成 multi-part response。
4. mutation 请求构造新的 `RtnetlinkState`，失败时返回 netlink error。
5. 成功更新后同步到 `Service`，带 `NLM_F_ACK` 时返回 ack。

## 并发模型

- 全局 `SERVICE` 是 `LazyInit<Mutex<Service>>`，串行化 smoltcp `Interface` 和 `Router` 访问。
- 全局 `SOCKET_SET` 内部使用 `Mutex<SocketSet>`，所有 smoltcp socket handle 访问都通过 `with_socket_mut` 串行化。
- `LISTEN_TABLE` 用 `Mutex<HashMap<...>>` 管理 listener，再用 per-entry `Mutex` 保护 backlog 队列。
- TCP 状态转换使用 `StateLock` 的 atomic CAS，失败时返回当前状态。
- socket option 和 shutdown 标志使用 atomics，跨线程读写只表达配置或关闭状态。
- Unix stream 的 ring buffer producer 与 consumer 被 `channel: Mutex<Option<Channel>>` 包住，send 与 recv 对同一 channel 的 ring 操作串行执行。
- netlink `ROUTE_STATE` 使用 `RwLock`，rx queue 和 subscriber 列表使用 `Mutex`。
- RX waker 由 `GeneralOptions::device_mask` 指向相关设备，`Service::register_rx_waker` 同时注册 timeout 和设备 IRQ waker。

## 设计决策

### smoltcp 作为协议引擎

smoltcp 已提供 no_std TCP、UDP、raw socket 和 interface poll 模型。
`knet` 在它之上补齐内核对象生命周期、Linux errno 映射、poller、listen backlog、rtnetlink 和设备 glue。
代价是 socket set 必须通过全局锁和 handle 间接访问，并且协议推进依赖显式 `poll_interfaces` 调用。

### 控制面状态与 data-plane 同步

rtnetlink mutation 先更新 `RtnetlinkState`，再同步到 `Service`、`Router`、smoltcp `Interface` 和 `NetDevice`。
这种结构让 netlink dump 有稳定的 ABI presentation，也让路由表和设备地址能通过同一状态来源重建。
代价是 dump 当前读取控制面状态，运行时统计、驱动瞬时状态和 smoltcp 内部计数没有完整反查。

### 设备 mask 驱动 RX 唤醒

每个 socket 根据 bind 或 connect 结果记录设备 mask。
等待 RX 时只向相关设备注册 waker，同时注册 smoltcp poll timeout。
这个设计减少无关设备中断唤醒，但依赖路由和地址同步保持 mask 准确。

### TCP listen 表独立于 smoltcp listener socket

监听 socket 和 accepted child socket 分开管理。
`ListenTable` 根据收到的 SYN 创建 child smoltcp socket，把待完成连接放入 backlog 队列。
这个设计让 POSIX accept 语义集中在 knet 内部，代价是 listen table 必须 snoop TCP 首包并清理 aborted child。

### Ethernet ARP pending queue

Ethernet 设备为未解析 next-hop 保留 `pending_tx`。
同一个 next-hop 的 ARP 回复到达后，设备按队头顺序发送等待中的 IP 包。
当前队列存在 head-of-line blocking，长时间 unresolved 的 next-hop 会阻塞后续 pending 包。

### vsock-TIPC bridge

`vsock_tipc_bridge` feature 打开时，bridge 在 vsock device 注册后初始化端口资源：
保存 vsock device、监听桥接端口并发布反向 TIPC port。
bridge worker 不在 device 注册回调里启动；
它们由 `kruntime` 在 SMP secondary run queue 注册完成后 late-start，
以避免 early boot 阶段把 task 调度到尚未注册的远端 CPU run queue。
它不创建普通 AF_VSOCK stream endpoint，而是在 `device/vsock.rs` 的 driver event router 处优先识别桥接端口和已桥接连接。
未命中的事件继续交给原有 `VSOCK_CONN_MANAGER`，保持普通 AF_VSOCK stream 行为。

host-to-TA 方向使用固定端口映射：

- port 0：动态 service-name handshake（见下节）。
- port 1：`com.android.trusty.keymint`
- port 2：`com.android.trusty.gatekeeper`
- port 3：`com.android.trusty.vsock.forwarder`
- port 4：`com.android.trusty.widevine.transact`

#### Port 0 动态 handshake 协议

Trusty-compatible dynamic bridge 在 host 连上 vsock port 0 后按 record 语义工作：

1. Host 发送**第一个 vsock record**：UTF-8 TIPC service path（无 NUL 终止），长度不超过 `IPC_PORT_PATH_MAX`。
2. Guest bridge 对该 path 调用 `ipc_port_connect_async(..., WAIT_FOR_PORT | ASYNC)`：
   - 若 service **尚未 publish**，channel 保持 `Connecting`，**不**向 host 回包；等 TA publish 且 TIPC READY 后再继续。
   - 若 service **已存在**或随后 publish 成功，bridge 在 TIPC READY 后向 host 发送单字节状态码 `[0]`，此后双方按 record 转发 payload。
3. 下列情况 guest 发送 `[1]` 并关闭 vsock（**明确拒绝**，不同于“等待 publish”）：
   - service name 非法（UTF-8 / 长度 / 内容校验失败）；
   - 读取 service name record 失败；
   - TIPC channel 在 `TipcConnecting` 阶段收到 `HUP` / `ERROR`（例如 port 存在但拒绝连接）。
4. Host 侧（`libtrusty`）在发出 service name 后阻塞读取状态字节；应设置 recv 超时（`TRUSTY_VSOCK_TIMEOUT_SEC`，默认 60s）。超时或 EOF 视为连接失败。收到 `[1]` 映射为 `-EIO`。
5. 若 service **永远不存在**，guest 会一直等待 publish，host 在 recv 超时后失败——负例测试依赖该超时，而非 `[1]`。

固定端口（1–4）在 vsock 连接建立时即 `WAIT_FOR_PORT | ASYNC` 连到预置 TIPC service，不使用上述单字节 handshake。

TA-to-host 方向发布 `com.android.trusty.vsock.forwarder` TIPC port，并把 accepted channel 连接到 host CID 2 port 0。
bridge worker 直接使用 `VsockDevice` 的 `listen/connect/recv/send/disconnect/abort`，并直接复用 TIPC core 的 `IpcChan`、`IpcPort` 和 `HandleSet`。

vsock `Received(conn_id, len)` 事件由 bridge 以 record 语义处理：
bridge 分配 `len` 大小的临时 buffer 并调用一次 `recv(conn_id, &mut buf[..len])`，然后把该 record 作为一个 TIPC message。
如果 TIPC send 返回 `WouldBlock`，bridge 只保留一条 pending record 并等待 `SEND_UNBLOCKED` 后重试。
TIPC 到 vsock 方向按 `get_msg -> read_msg -> put_msg -> send` 转发；
v1 不排队等待 vsock credit，send 失败会关闭连接。
bridge v1 只转发 bytes，不转发 TIPC handles 或 memrefs。

**事件路由约束**：`route_event` 对 `ConnectionRequest`（mapped port）立即记入 `pending_inbound`，使同批 `Received`（port 0 service name）在 `BridgeConnection` 创建前也能入 bridge FIFO。**不得**仅凭 `bridge_mapping(local_port)` 认领所有 `Connected`/`Received`，否则 guest 普通 AF_VSOCK 若显式 `bind(1..4)` 再 `connect()`，事件会被 bridge 误消费。

`vsock-poll` 可能在同一批 `poll_event` 中连续弹出 `ConnectionRequest` 与 `Received`；若丢弃 `Received`，动态 handshake 的 service-name record 永久丢失，host `tipc_connect` 会在 status byte 上超时（`EAGAIN` / `-11`）。

`bind()` 在 `vsock_tipc_bridge` 开启时拒绝显式绑定 `BRIDGE_PORT_MAP` 端口（ephemeral 分配本就跳过这些端口）。

反向（TIPC→vsock）连接使用 `allocate_reverse_port()`，不在 `BRIDGE_PORT_MAP` 中；`accept_reverse_tipc()` 在 `dev.connect()` 之前插入 `BridgeConnection`，`has_connection()` 为真时生命周期事件仍走 bridge。

## Drop / 资源释放

- `SocketSetWrapper::remove` 按 smoltcp handle 移除 socket。
- TCP listener 关闭时，`ListenTable::unlisten` 标记 entry closed，drain child handles，并从 `SOCKET_SET` 删除。
- Unix stream listener 在 `Drop` 中清空 bind slot，释放 pending connection request。
- Unix stream channel 被 `Option<Channel>` 持有，双方 shutdown 后取出 channel 并唤醒 peer。
- UDP socket 通过 UDP error registry 注册异步错误状态，Drop 路径应保持 unregister 与 handle 生命周期一致。
- Ethernet RX buffer 在 `poll_rx` 完成 frame 处理后调用 driver `recycle_rx` 归还。
