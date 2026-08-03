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
│  ├─ Unix stream ring buffer │
│  └─ vsock RX ring advance   │
└──────────────┬──────────────┘
               │
               v
driver layer / smoltcp / ringbuf
```

- safe API 调用者信任 `knet` 维护 socket 状态、路由状态、buffer 边界和 errno 映射。
- `knet` 信任 `posix/net` 完成用户指针读取、sockaddr 长度校验、地址族选择和权限策略。
- pathname Unix socket 操作信任入口传入的 `Cred` 快照与当前系统调用主体一致；
  kvfs 负责逐级 search、父目录 mutation 和 inode DAC 检查。
- `knet` 信任 driver 层返回的 `NetBufHandle` 数据切片在 handle 生命周期内有效。
- `knet` 使用基于 `zerocopy` 和 `etherparse` 的 crate 内 checked parser 校验 Ethernet、ARP、IPv4 和 UDP 数据，并继续依赖 smoltcp 校验 TCP、raw IP 和 IPv6 数据。
- 设备层只接收经 `Router` 控制面适配后的 crate 内地址、CIDR 和邻居项。
- unsafe 边界由 crate 内部封装，外部调用者没有直接调用 unsafe API 的入口。

## 外部边界 / 攻击面

`knet` 是面向外部输入最丰富的 crate 之一，
攻击面不仅来自 Rust `unsafe`，
还来自网络包、控制面消息、设备缓冲区和上层 syscall glue。

经检查，本模块直接或间接接触以下边界：

- **网络输入**：Ethernet、ARP、IPv4、TCP、UDP、raw IP 包；
- **设备输入**：driver 提供的 `NetBufHandle`、IRQ 唤醒、RX/TX 缓冲区生命周期；
- **控制面输入**：netlink header、attribute、route mutation、neighbor 更新；
- **Unix / vsock 输入**：peer socket 数据、连接建立与关闭事件，
  以及 pathname Unix socket 经 kvfs 解析的路径和 inode；
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

### 1. Unix stream 写入 vacant ring buffer

位置：`src/unix/stream.rs:388` 和 `src/unix/stream.rs:392`

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

### 2. Unix stream 推进 write index

位置：`src/unix/stream.rs:407`

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

### 3. Unix stream 推进 read index

位置：`src/unix/stream.rs:440`

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

### 4. vsock 推进 RX read index

位置：`src/vsock/connection_manager.rs:151`

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

### 5. UDP waiter 测试专用 `RawWaker`

位置：`src/transport/udp/wait.rs:97-124`

该实现只在 `cfg(unittest)` 下编译，用于统计 `PollSet` 的唤醒次数。
`RawWaker` 数据指针始终来自 `Box::leak` 生成的静态 `AtomicUsize`，
vtable 回调保持原指针与静态生命周期，不释放该测试计数器。
生产构建不包含这组 unsafe 代码。

## 内存安全不变量

1. `SocketHandle` 生命周期：任何 smoltcp socket handle 在访问前必须仍存在于 `SOCKET_SET`。
2. `SocketSet` 互斥访问：所有 `SocketSet` 读写必须经过 `SOCKET_SET.inner` 的 mutex。
3. `Service` 互斥访问：smoltcp `Interface`、`Router` 和设备 dispatch 必须在 `SERVICE.lock()` 内推进。
4. ring buffer publish 规则：Unix stream 和 vsock 只能发布已经写入的字节，只能消费当前 occupied 区域内的字节。Unix stream 的 write index 发布、方向关闭和空队列 EOF 判定由同一个方向锁排序。
5. driver buffer 生命周期：`NetBufHandle::data` 只在 handle 被 recycle 前使用，RX handle 必须在处理后归还。
6. netlink message 边界：所有 payload 读取必须先经过 header 长度、attribute 长度和 family 校验。
7. route index 边界：rtnetlink route 的 `oif` 转换成设备索引后必须检查 `dev < devices.len()`。
8. static init 顺序：创建 socket 前必须完成 `init_network` 初始化 `SERVICE`、`SOCKET_SET` 和 `LISTEN_TABLE`。
9. pathname credential 一致性：一次 Unix pathname bind 的查找、创建和属主初始化必须使用同一份 `Cred` 快照。
10. kernel caller 边界：没有当前用户任务的内核调用者不得进入隐式 `current_cred()` 路径；pathname 操作和 socket file 构造都必须显式选择凭据。
11. `PacketBuf` 所有权：设备、Router、loopback 和 smoltcp adapter 之间按值转移报文；协议偏移只能落在当前有效数据范围内。
12. IPv4 输入边界：本地交付前必须校验版本、头长、总长和头部校验和，并按 `total_len` 截断尾部数据。
13. 网络类型边界：`RouteTable` 和 `NetDevice` 不暴露 smoltcp 地址或时间类型；控制面与协议兼容转换由 `Router`、`Service` 和初始化入口完成。
14. IPv4 重组边界：分片按源地址、目的地址、标识、协议和接口隔离；被已有区间完全覆盖的分片按重复包丢弃，部分重叠或范围矛盾会删除整条队列；重组状态受 64 条队列、4 MiB 高水位、3 MiB 低水位和 30 秒超时限制。
15. UDP 接收边界：UDP 长度、校验和和 payload range 通过 checked parser 验证，单个 PCB 的接收队列上限为 1024 个数据报。
16. IPv4 输出边界：输出 MTU 来自匹配路由；DF 包超过 MTU 时返回 `EMSGSIZE`，允许分片的包只按 8 字节对齐切分 payload。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `TcpSocket` | 字段满足 Send | 内部锁、atomic 和 global socket set 串行化共享状态 |
| `UdpSocket` | 字段满足 Send | `Arc<UdpPcb>` 内的锁、atomic 和分桶 PCB registry 保护共享状态 |
| `RawSocket` | 字段满足 Send | `RwLock`、atomic 和 immutable handle 保护共享状态 |
| `StreamTransport` | 字段满足 Send | `Mutex<Option<Channel>>` 串行化本端操作，per-direction `SpinNoPreempt` 排序数据发布、半关闭与 EOF 判定，三组 `PollSet` 隔离读、写和连接状态 waiter |
| `NetlinkSocket` | 字段满足 Send | `Arc<NetlinkSocketInner>` 内部使用 `RwLock`、`Mutex` 和 `PollSet` |
| `Service` | 在 `Mutex<Service>` 内使用 | 通过全局 `Mutex` 提供共享访问 |
| `Router` | 在 `Service` 内使用 | 通过 `Service` mutex 间接共享 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 初始化顺序错误导致 `LazyInit` 访问未初始化对象 | 高 | socket 在 `init_network` 前创建或 poll | 启动路径由 `core/kruntime` 先调用 `init_network`；新增入口需保留该顺序 |
| T-02 | smoltcp socket handle 已删除后继续访问 | 高 | listener 关闭、accepted child 清理与并发 socket 操作交错 | `SocketSet` 访问由 mutex 串行化；`ListenTable::unlisten` drain 后删除 handle；调用点需避免缓存 handle 后跨释放点使用 |
| T-03 | ring buffer index 推进超过实际写入或读取长度 | 高 | `advance_write_index` 或 `advance_read_index` 的 count 与切片来源不一致 | count 由持有 channel mutex 时取得的同一批 slices 计算；unsafe 注释固定不变量 |
| T-04 | 恶意 netlink 消息越界读取或构造非法状态 | 高 | 用户传入短 header、畸形 attr、非法 family、非法 ifindex | `NlMsgHeader::read`、`parse_attrs`、`parse_ip_by_family` 和 route index 检查拒绝非法输入 |
| T-05 | ARP spoofing 污染 neighbor cache | 中 | 外部主机发送伪造 ARP reply 或 request | 当前校验 unicast MAC、广播和本机目标 IP；完整邻居安全策略仍依赖网络隔离 |
| T-06 | listen backlog 被 SYN 洪泛占满 | 中 | 大量连接请求命中同一 listener | backlog 被 clamp 到 `LISTEN_QUEUE_SIZE`；超限丢弃并记录 warn |
| T-07 | raw socket 被无权限调用者创建 | 中 | syscall 层没有实施权限门禁 | `knet` 层只封装 raw socket 行为；权限策略应保留在 `posix/net::sys_socket` |
| T-08 | netlink RX queue 被 uevent 或 response 填满 | 中 | 订阅者不消费，publisher 持续写入 | `NETLINK_RX_QUEUE_LIMIT` 限制单 socket queue 字节数，超限丢弃 |
| T-09 | 路由状态与 data-plane 不一致 | 中 | rtnetlink mutation 只更新控制面或同步过程中出错 | `update_route_state` 写入 `ROUTE_STATE` 后调用 `SERVICE.lock().sync_netlink`；新增 mutation 需复用该路径 |
| T-10 | 外部网络包触发 parser panic | 中 | malformed Ethernet、ARP、IP、UDP 或 TCP packet 进入 RX | Ethernet 和 ARP 使用 `zerocopy` checked view，IPv4 与 UDP 使用 crate 内 checked parser，TCP、raw IP 和 IPv6 使用 smoltcp checked parser；错误包直接丢弃 |
| T-11 | 中断上下文误用导致锁竞争或延迟放大 | 中 | IRQ waker 回调中直接推进 `SERVICE`、`SOCKET_SET` 或执行阻塞 socket 操作 | 中断路径只注册和唤醒 waker，协议推进保留在普通 poll 路径 |
| T-12 | driver buffer 或 DMA 输入破坏 packet 边界 | 高 | 驱动返回长度异常、数据在 recycle 后继续被访问、TX/RX buffer 生命周期使用错误 | RX 数据只在 `NetBufHandle` recycle 前解析和复制；外部帧使用 checked parser；TX buffer 由 driver handle 管理 |
| T-13 | vsock-TIPC bridge 误把普通 AF_VSOCK 连接路由到 TIPC | 中 | 事件分流没有区分桥接端口或已桥接连接 | bridge 只接管静态 port map 和自己的 connection id，未命中事件继续交给 `VSOCK_CONN_MANAGER` |
| T-14 | host 通过 bridge 注入超大或非法 TIPC message | 中 | `Received` record 超过 TIPC slot 或 port 0 service name 非法 | bridge 限制 record 长度为 `IPC_CHAN_MAX_BUF_SIZE`，动态 service name 需通过 UTF-8、NUL 和长度校验；非法 name 回 `[1]` 并断开 |
| T-15 | TIPC handle/memref capability 经 vsock 泄露到 host | 高 | TA 向 bridge 发送带 attached handles 的 message | bridge v1 只转发 bytes，发现 attached handles 时关闭连接 |
| T-16 | host 误判 port 0 handshake 结果 | 中 | 未读状态字节就发 payload；忽略 `[1]`；无 recv 超时导致永久阻塞 | 协议要求 host 先读单字节状态（`0`=成功，`1`=拒绝）；`libtrusty` 使用 `SO_RCVTIMEO`；CA 测试拒绝非 `[0]` 状态 |
| T-17 | IPv4 分片耗尽内核内存 | 中 | 外部持续发送无法完成重组的不同分片流 | 重组器限制队列数量与总内存，超过高水位后淘汰最早队列到低水位，队列存活时间固定为 30 秒 |
| T-18 | 重叠 IPv4 分片混淆上层解析 | 中 | 同一重组 key 提交相互覆盖的 payload range | 被已有区间完全覆盖的分片按重复包丢弃，任何部分重叠或总长度矛盾会删除整条队列 |
| T-19 | UDP 接收洪泛占满 socket 队列 | 中 | 应用读取速度低于入包速度 | 每个 PCB 最多保留 1024 个数据报，满队列丢弃新数据报并保持内存上限 |
| T-20 | pathname Unix socket 绕过 inode/目录 DAC 或复用已有 inode | 高 | bind/connect/sendto 直接访问 binding 表，或 bind 接受已有路径 | bind 通过 `parent_at` 和 `Path::mknod` 排他创建；connect/sendto 在 lookup 后检查最终 inode `MAY_WRITE`；abstract 地址才直接访问内存 binding 表 |
| T-21 | 内核任务隐式读取用户凭据 | 高 | 启动期 pathname bind 调用普通 `SocketOps::bind`，当前线程不存在或主体错误 | 内核调用者使用 `bind_with_cred` 显式传入 `initial_cred()` 等已选择凭据；普通入口只服务当前用户任务 |
| T-22 | Unix stream 在 EOF 后发布数据 | 中 | send、shutdown 与 peer recv 并发交错，关闭状态和 write index 缺少共同排序 | 每个发送方向使用共享 `tx_order`；send 在锁内复检后发布，recv 在锁内复查 empty 和 closed，Channel 释放前先发布关闭状态 |

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
| F-05 | UDP ICMP error queue 丢失 | PCB registry 未注册、ICMP 引用头无效或 socket 已关闭 | `SO_ERROR` 或 error queue 缺失 | 应用无法获得异步网络错误 | 4 | socket 创建时初始化 PCB registry，bind 时登记 PCB，Drop 时注销；ICMP 引用报文使用允许截断 payload 的 IPv4 头解析路径 |
| F-06 | TCP accepted child abort | 握手期间 peer reset 或 smoltcp child 关闭 | `accept` 返回 `ConnectionAborted` | 应用重试 accept | 4 | `ListenTable::accept` 清理 closed child 并继续扫描队列 |
| F-07 | poll waker 丢失 | device mask 错误、timeout 或 Ethernet IRQ source 未注册、注册所有者未跨 `Pending` 存活或缺少 register 后复查 | socket 阻塞等待延迟 | 应用 IO latency 上升或连接超时 | 3 | bind/connect 后更新 device mask；`Service::register_rx_waker` 使用同一个 `PollContext` 注册 timeout poll 和相关 Ethernet IRQ source；调用方持有 `PollRegistrations` 并在注册后复查 readiness |
| F-08 | malformed netlink request | header 或 attr 长度非法 | 返回 empty response 或 netlink error | 调用者请求失败 | 4 | checked reader 和 error response 处理 |
| F-09 | ROUTE_STATE 未初始化 | netlink route 请求早于 `init_route_state` | panic 或空状态 | netlink 功能不可用 | 2 | 初始化路径在 `init_network` 中创建初始 state；新增启动路径需保持顺序 |
| F-10 | Unix stream peer 提前关闭 | channel 被 shutdown 或 drop | send 在无发送进度时返回 `BrokenPipe`，已有进度时返回部分字节数；recv 排空本端缓冲后返回 EOF；peer 关闭时丢弃未读输入则返回一次 `ConnectionReset`；poll 报告 `RDHUP`、`HUP` 或 `ERR` | 应用感知连接关闭 | 4 | endpoint atomic 记录双方半关闭状态与待处理 reset；per-direction `tx_order` 统一关闭与数据顺序；三组 `PollSet` 按受影响事件定向唤醒 |
| F-11 | 中断上下文执行重型网络推进 | IRQ 路径误调用 socket send、recv 或 `poll_interfaces` | 锁竞争、调度延迟或死锁 | 网络 IO 延迟上升，严重时系统卡顿 | 2 | IRQ 路径只做 waker notification，实际协议推进由 task 上下文执行 |
| F-12 | RX buffer recycle 顺序错误 | frame payload 在 `recycle_rx` 后仍被引用 | 读取悬垂数据或数据损坏 | packet 解析异常，严重时破坏内存安全 | 1 | `EthernetDevice::poll_rx` 在 recycle 前完成解析和复制；新增设备适配需保持同样生命周期 |
| F-13 | port 0 handshake 永久等待 | host 早于 TA publish 连接且未设 recv 超时；或 service 永不 publish | host `read` 阻塞；负例测试挂起 | CA/测试进程无响应 | 3 | dynamic connect 保留 `WAIT_FOR_PORT`；host 设 `TRUSTY_VSOCK_TIMEOUT_SEC`；明确拒绝场景回 `[1]` |
| F-14 | 快速重连 `tipc_connect` 超时 `-11` | `route_event` 在 `has_connection()` 前丢弃同批 `Received`，service-name record 丢失 | host status-byte `EAGAIN`；约半数快速重连失败 | storage client/proxy harness 间歇失败 | 2 | mapped bridge port 仅按 `local_port` 认领；事件入 FIFO，不依赖 `has_connection()` |
| F-15 | IPv4 分片重组超时 | 首片到达后 30 秒内缺少后续分片 | 当前数据报丢失 | UDP 接收超时 | 3 | 删除过期队列；首片存在且允许回复时发送 ICMPv4 Fragment Reassembly Timeout |
| F-16 | UDP DF 数据报超过路由 MTU | `IP_MTU_DISCOVER` 要求 DF 且 packet 长度超过路由 MTU | 当前发送失败 | 应用收到 `EMSGSIZE` | 4 | 发送前读取路由 MTU，Router 拒绝对 DF 包执行输出分片 |
| F-17 | 启动期 Unix pathname bind panic | 内核任务调用隐式 `current_cred()`，但尚无当前用户线程 | `/dev/log` 等内核 socket 无法绑定 | 启动中断 | 2 | 启动期调用 `bind_with_cred` 并显式传入 `initial_cred()`；保留可用的初始 fs context |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- 普通输入错误使用 `KError` 和 `LinuxError` 返回，例如 `EINVAL`、`EAFNOSUPPORT`、`ENETUNREACH`、`EADDRINUSE`、`EWOULDBLOCK`。
- malformed netlink 请求返回 netlink error response，短到无法读取 header 的请求返回空 response。
- malformed Ethernet、ARP、IP、UDP、TCP 包在 RX 路径丢弃，并通过 warn 或 trace 记录。
- UDP PCB 接收队列和 Router TX 队列满时映射为丢包或 `WouldBlock`，poller 负责等待 IO readiness。
- smoltcp buffer 和 Unix stream ring buffer 满时映射为 `WouldBlock`，poller 负责等待 IO readiness；非阻塞 Unix stream send 已有进度时返回部分字节数。
- loopback 和 Ethernet 队列满时丢包并记录 warn。
- panic 路径主要来自初始化顺序、内部 invariant 破坏和 `expect` 断言；新增公开入口应先返回 `KError`，再进入内部断言区。

## 隐私分析

`knet` 会处理用户进程通过 socket 发送的 payload、从网络收到的 packet payload、Unix socket credentials、netlink 消息和 vsock payload。
这些数据在内核内按 socket buffer、ring buffer、driver buffer 或 netlink queue 保存。
模块自身不做持久化，也不把 payload 写入日志；trace 日志当前会输出 Ethernet frame 字节，生产环境需按日志级别控制敏感网络数据泄露。

## 已知限制

- `RTM_GETNEIGH` dump 尚未实现，neighbor 只通过 `RTM_NEWNEIGH` mutation 进入控制面。
- IPv4 输出使用匹配路由的接口 MTU，ICMP Fragmentation Needed 中的 next-hop MTU 只进入 UDP error queue，尚未形成动态 PMTU cache。
- IPv4 输出分片只支持无 options 的栈内生成报文，尚未实现 options copy 语义。
- `ROUTE_STATE` dump 没有覆盖所有 live runtime 状态。
- raw socket 创建权限由 syscall 层承担，`knet` 构造器自身没有进程凭据参数。
- Ethernet 设备只处理 IPv4 ARP，IPv6 NDP、非 Ethernet 链路和多队列 NIC 抽象仍待扩展。
- crate 内 UDP 数据路径当前只支持 IPv4；IPv6 UDP 继续由 smoltcp DNS 路径使用，普通 UDP socket 不提供 IPv6 收发。
- vsock-TIPC bridge v1 不转发 TIPC handles 或 memrefs，也不为 vsock send credit 建立持久重试队列。

## 审计清单

修改本模块时需验证：

- 每个 `unsafe` 块均有 `SAFETY:` 注释。
- 新增 smoltcp socket handle 的生命周期受 `SOCKET_SET` 保护。
- 新增 route 或 device mutation 通过 `update_route_state` 或等效同步路径更新 data-plane。
- 新增 netlink parser 先校验 header 长度、attribute 长度、family 和 index。
- 新增 ring buffer 操作的 advance count 来自同一锁内同一批 slices。
- Unix stream 的 write index 发布、方向关闭和空队列 EOF 判定保持同一个 per-direction 排序点，方向锁内不执行用户复制或 `PollSet` 唤醒，shutdown 在释放 `channel` mutex 后执行 waiter 唤醒。
- 新增外部网络输入使用 checked parser。
- IPv4 分片重组改动保持队列数量、内存和超时上限，并拒绝重叠 range。
- UDP registry 改动保持 bind、connect、普通接收和 ICMP error lookup 使用同一 PCB 所有权来源。
- 新增 socket option 明确 errno、阻塞语义和 poll readiness。
- 新增 pathname Unix socket 入口使用单次凭据快照，并让全部 VFS 操作显式接收该快照。
- pathname bind 保持排他创建、`0777 & !umask` mode 和 fs credential owner；connect/sendto
  在读取 binding 前检查最终 inode `MAY_WRITE`。
- 新增内核调用路径不得依赖 `current_cred()`；调用者必须显式选择凭据。
- 新增公开 API 先确认是否需要跨 crate 暴露。
