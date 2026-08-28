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
- `knet` 信任 `posix/net` 完成用户指针读取、sockaddr 长度校验和地址族选择。
  netlink send 路径还信任 syscall 与 socket file 入口传入本次调用者的 `Cred` 快照，
  mutation 权限检查由 `knet` 执行。
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
- **控制面输入**：每次发送携带的调用者凭据、批量 netlink message、header、
  attribute、route mutation、neighbor 更新；
- **Unix / vsock 输入**：peer socket 数据、连接建立与关闭事件，
  以及 pathname Unix socket 经 kvfs 解析的路径和 inode；
- **上层 syscall 语义输入**：由 `posix/net` 传入的 socket 地址族、
  socket option、阻塞语义和权限决策结果。

本模块不直接解引用用户指针，
而是信任 `posix/net` 先完成用户内存访问和长度校验。netlink 调用者凭据由
POSIX send 或 socket file write 入口取得，并显式传递给 `NetlinkSocket`。
本模块也不直接执行 DMA 编程，
但会消费由 driver 层提供的网络缓冲区，
因此仍需把 driver buffer 生命周期视为关键边界。

因此威胁分析重点应覆盖：

- 外部网络包是否能触发越界、panic、状态不一致或资源耗尽；
- link 与 neighbor mutation 是否绕过设备 owner，address 与 route mutation 是否绕过 Router owner，设备注销是否与 rtnetlink mutation 使用同一把配置锁；
- netlink mutation 是否复用旧凭据，或在混合批次和 queue 耗尽时产生部分提交；
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
3. `Service` 互斥访问：设备 RX drain、路由状态、ingress queue 和 TX dispatch 必须经过内部 Router mutex，smoltcp Interface 必须经过独立 mutex；IPv4 校验和 snoop 在 Router 锁外执行。
4. ring buffer publish 规则：Unix stream 和 vsock 只能发布已经写入的字节，只能消费当前 occupied 区域内的字节。Unix stream 的 write index 发布、方向关闭和空队列 EOF 判定由同一个方向锁排序。
5. driver buffer 生命周期：`NetBufHandle::data` 只在 handle 被 recycle 前使用，RX handle 必须在处理后归还。
6. netlink message 边界：所有 payload 读取必须先经过单条 header 长度、批次对齐边界、attribute 长度和 family 校验。
7. netlink credential 边界：每次 `send` 或 `write` 必须传入当前调用者的独立 `Cred` 快照，netlink socket 不得缓存调用者权限。
8. netlink batch 执行边界：每个 socket 的发送事务锁串行化容量预检、mutation 执行和 response 入队；`rtnl_lock` 串行化跨 socket mutation 与 `unregister_netdev`；混合查询和 mutation 的批次必须在状态更新前拒绝；同类 mutation 执行前必须检查完整批次的 response queue 空间。response 在 rx queue 锁外生成，锁顺序固定为发送事务锁、`rtnl_lock`、Router、ingress、Interface、netlink rx queue，未涉及的锁按序跳过。
9. route index 边界：rtnetlink route 的 `oif` 转换成设备索引后必须检查 `dev < devices.len()`。
10. static init 顺序：创建 socket 前必须完成 `init_network` 初始化 `SERVICE`、`SOCKET_SET` 和 `LISTEN_TABLE`。
11. pathname credential 一致性：一次 Unix pathname bind 的查找、创建和属主初始化必须使用同一份 `Cred` 快照。
12. kernel caller 边界：没有当前用户任务的内核调用者不得进入隐式 `current_cred()` 路径；pathname 操作和 socket file 构造都必须显式选择凭据。
13. `PacketBuf` 所有权：报文进入网络栈时创建指针大小的引用计数句柄，设备、Router、loopback、PCB 和 smoltcp adapter 之间按值转移该句柄；共享后的修改执行写时复制，协议偏移和已校验 UDP payload range 只能落在当前有效数据范围内。
14. IPv4 输入边界：本地交付前必须校验版本、头长、总长和头部校验和，并按 `total_len` 截断尾部数据。
15. 网络类型边界：`RouteTable` 和 `NetDevice` 不暴露 smoltcp 地址或时间类型；控制面与协议兼容转换由 `Router`、`Service` 和初始化入口完成。
16. IPv4 重组边界：分片按源地址、目的地址、标识、协议和接口隔离；被已有区间完全覆盖的分片按重复包丢弃，部分重叠或范围矛盾会删除整条队列；重组状态受 64 条队列、4 MiB 高水位、3 MiB 低水位和 30 秒超时限制。
17. UDP 接收边界：UDP 长度、校验和和 payload range 通过 checked parser 验证，单个 PCB 的接收队列上限为 1024 个数据报。PCB 创建时预留该上限对应的指针大小 `PreparedUdpPacket` 槽位；loopback 发送侧必须在关闭 BH 前完成数据报解析，并把结果写入已有 `PacketBuf` 的控制元数据。`NetRx`/`SpinNoIrq` 入队路径只能移动已有句柄，不得分配或扩容。队列满时先释放 PCB 队列锁，再丢弃当前数据报。`MSG_PEEK` 只能在锁内增加 `PacketBuf` 引用计数，payload 复制必须在 `SpinNoIrq` 外完成。
18. IPv4 输出边界：栈内 UDP 的输出 MTU 来自匹配路由指向的设备；UDP 与 raw IPv4 在路由缺失或输出设备管理 down 时返回 `ENETUNREACH`，DF 包超过 MTU 时返回 `EMSGSIZE`，允许分片的包只按 8 字节对齐切分 payload。
19. poller 完成条件：唯一执行者通过一次 CAS 从 `RUNNING` 或 `RUNNING_PENDING` 发布 `IDLE` 或 `SCHEDULED`；RX、ingress、TX 和已到期 timer 均无立即工作且执行期间没有通知时才能进入 `IDLE`。每轮 bulk work 受 1 ms 软时间上限约束，kwork poller callback 执行有界批次，每个批次最多推进四轮，达到上限且仍有立即工作时保留 `SCHEDULED` 并 queue 下一批。
20. 协议 timer 条件：每轮 `poll_maintenance`、`poll_ingress_single` 和 `poll_egress` 完成后必须用 `poll_at` 刷新下一次 deadline；尚未到期的 timer 不进入 `PollProgress::has_more`，到期后必须通过 `notify(PollReason::Timer)` 发布工作。
21. TX 与接收窗口交接条件：socket 或 Router 产生 TX 工作后必须通过 `notify(PollReason::Tx)` 发布；TCP 从余量低于最大窗口缩放量子的接收缓冲区消费数据后必须通过 `notify(PollReason::RxWindow)` 发布零窗口恢复工作；Router data TX queue 容量实际增加时必须设置 `tx_capacity_changed`，TX waiter 只依据该字段执行全局容量唤醒。
22. TCP 关闭生命周期：用户文件对象释放后，含有待发送数据、FIN 或重传状态的 smoltcp handle 必须继续保留在 `SOCKET_SET`；接收队列存在未读数据时必须保留 abort handle 直到 RST 发出；协议进入 `Closed` 后才能删除。只有 orphan `FIN_WAIT_2` 状态受 60 秒回收期限约束。
23. 网络 timer 条件：协议与 deferred-close deadline 只能通过原子值发布；周期采样回调到期处理不得获取 sleepable mutex、分配 future 或访问 timer wheel。
24. link 配置所有权：接口名、MTU、管理 up 状态和 link snapshot 只由 `NetDevice` 持有；`RTM_NEWLINK` 必须先完成名称唯一性、名称格式、设备 MTU 范围和受支持 flags 的整组校验，再修改设备。`ifi_change` 中的派生状态位按 Linux `IFF_VOLATILE` 语义保留，实际改变的未支持可写位返回 `EOPNOTSUPP`。
25. IPv4 地址生命周期：地址或所属设备移除后，Router 地址条目、自动路由、IngressProcessor、smoltcp Interface 和设备投影必须在同一 Router 锁作用域内刷新；最后一个同值地址消失时，Router 中依赖该 `prefsrc` 的配置路由必须同步删除；设备消失时还必须删除其路由与邻居并重编号后续接口索引。
26. poller 锁竞争交接条件：持有全局推进权时，Router、IngressProcessor、smoltcp Interface 和 socket-set 只能通过 `try_lock` 获取；accepted batch 交接必须按 socket-set、Router 的顺序获取两把锁，只有 control batch 时跳过 socket-set；竞争必须返回 `has_more`。已经从设备取出的 raw batch、等待进入 smoltcp 的 accepted batch 和待发送 control batch 必须保留到成功交接，TCP listener preparation 与 accepted batch 入队必须处于同一次成功交接中。
27. loopback UDP 交付边界：loopback xmit 必须在关闭 BH 前给完整 IPv4 UDP 盖戳，再于 `local_bh_disable` 下把同一 `PacketBuf` 放入共享 `NET_RX_QUEUE` 的 `pending_udp` 并 raise `NetRx`；`NetRx` 只取出 `pending_udp` 中的已盖戳 UDP，不查找 `LoopbackDevice`。`NetRx` 与 task fallback 只能通过 `SpinNoIrq` 查找 UDP PCB 并入队。完整 IPv4 UDP 不得依赖 poller `RUNNING` 所有权。TCP、ICMP、IPv6、分片和未命中 socket 的 UDP 由 `poll_rx` 从 `deferred` 交给任务 poller。
28. `NET_RX_QUEUE` 容量与所有权边界：`pending_udp` 与 `deferred` 必须在设备创建时按 `SOCKET_BUFFER_SIZE` 预分配，`enqueue` 以两条队列长度与 in-flight 合计为上限拒绝超额报文，因此 `push_back` 不得在 `SpinNoIrq` 或 `NetRx` softirq 内触发扩容。`process_pending` 取出的盖戳 UDP 必须计入 in-flight 预留，批次大小不超过 `NET_RX_BUDGET`，在锁外交付；未命中 PCB 的报文必须仍唯一持有句柄，清除元数据后放入 `deferred`。队列满时 `enqueue` 必须把报文交还调用方，由发送路径在释放 `NET_RX_QUEUE` 与 BH guard 之后再 drop；`discard_ifindex` 必须先把匹配报文移出到局部容器，解锁后再释放，避免在 `SpinNoIrq` 内堆释放。发送路径的 task fallback 只处理进入时已有的 `pending_udp`，并按 `NET_RX_BUDGET` 拆成固定轮次。队列按 `ifindex` 区分报文所有者，`LoopbackDevice::drop` 必须调用 `discard_ifindex`；该清理对 `process_pending` 释放锁期间的在途报文是 best-effort，完整的设备身份语义需要等价于 Linux `skb->dev` 与 `NETREG_UNREGISTERING` 的注册状态。
29. TCP listener 推进条件：收到 TCP 报文或 SYN 队列仍有 child 时，下一次实际网络 poll 必须刷新对应 listener。child 进入 accept 队列后才唤醒 `accept_poll`，唤醒发生在 entry 锁外。SYN 队列非空不会设置 `PollProgress::has_more`，未到期的协议 timer 继续等待 deadline 通知，避免半连接造成 poller 空转。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `TcpSocket` | 字段满足 Send | 内部锁、atomic 和 global socket set 串行化共享状态 |
| `UdpSocket` | 字段满足 Send | `Arc<UdpPcb>` 内的锁、atomic 和分桶 PCB registry 保护共享状态 |
| `RawSocket` | 字段满足 Send | `RwLock`、atomic 和 immutable handle 保护共享状态 |
| `StreamTransport` | 字段满足 Send | `Mutex<Option<Channel>>` 串行化本端操作，per-direction `SpinNoPreempt` 排序数据发布、半关闭与 EOF 判定，三组 `PollSet` 隔离读、写和连接状态 waiter |
| `NetlinkSocket` | 字段满足 Send | `Arc<NetlinkSocketInner>` 内部使用 `RwLock`、发送事务 `Mutex`、rx queue `Mutex` 和 `PollSet`；每次发送显式携带调用者凭据，socket 不保存权限主体 |
| `Service` | 字段满足 Send | 通过内部 `Mutex` 分别保护 Interface、Router、IngressProcessor、timeout 和可复用 batch；poller 以非阻塞方式取得共享推进锁并保留竞争期间的 batch；网络 timer 与 deferred-close deadline 使用原子值发布，不在周期采样回调路径获取 sleepable mutex |
| `Router` | 在 `Service` 内使用 | 通过 `Service` 的 Router mutex 间接共享 |

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
| T-09 | 控制面与 data-plane 不一致 | 中 | link mutation 更新设备 owner 失败，跨 socket 地址 mutation 交错，设备移除后保留旧地址投影或控制面路由，删除地址后配置路由继续引用失效 `prefsrc`，或 route 与 neighbor mutation 绕过各自 owner | `rtnl_lock` 串行化跨 socket 更新与 `unregister_netdev`；link query 与 mutation 直接访问 `NetDevice` owner；address 与 route query 和 mutation 直接访问 Router；`RTM_NEWNEIGH` 直接访问目标设备；地址与设备删除刷新自动路由、ingress、Interface 和设备投影；地址最后持有者和设备删除路径同步清理 Router 路由并更新接口索引 |
| T-10 | 外部网络包触发 parser panic | 中 | malformed Ethernet、ARP、IP、UDP 或 TCP packet 进入 RX | Ethernet 和 ARP 使用 `zerocopy` checked view，IPv4 与 UDP 使用 crate 内 checked parser，TCP、raw IP 和 IPv6 使用 smoltcp checked parser；错误包直接丢弃 |
| T-11 | 中断上下文误用导致锁竞争或延迟放大 | 中 | IRQ waker 回调中直接推进 `SERVICE`、`SOCKET_SET` 或执行阻塞 socket 操作 | VirtIO IRQ handler 只确认中断并调度 NetRx softirq；NetRx softirq 只标记/唤醒 RX source 并 queue `knet-poller` work；kwork callback 在普通任务上下文执行最多四轮的有界批次 |
| T-12 | driver buffer 或 DMA 输入破坏 packet 边界 | 高 | 驱动返回长度异常、数据在 recycle 后继续被访问、TX/RX buffer 生命周期使用错误 | RX 数据只在 `NetBufHandle` recycle 前解析和复制；外部帧使用 checked parser；TX buffer 由 driver handle 管理 |
| T-13 | vsock-TIPC bridge 误把普通 AF_VSOCK 连接路由到 TIPC | 中 | 事件分流没有区分桥接端口或已桥接连接 | bridge 只接管静态 port map 和自己的 connection id，未命中事件继续交给 `VSOCK_CONN_MANAGER` |
| T-14 | host 通过 bridge 注入超大或非法 TIPC message | 中 | `Received` record 超过 TIPC slot 或 port 0 service name 非法 | bridge 限制 record 长度为 `IPC_CHAN_MAX_BUF_SIZE`，动态 service name 需通过 UTF-8、NUL 和长度校验；非法 name 回 `[1]` 并断开 |
| T-15 | TIPC handle/memref capability 经 vsock 泄露到 host | 高 | TA 向 bridge 发送带 attached handles 的 message | bridge v1 只转发 bytes，发现 attached handles 时关闭连接 |
| T-16 | host 误判 port 0 handshake 结果 | 中 | 未读状态字节就发 payload；忽略 `[1]`；无 recv 超时导致永久阻塞 | 协议要求 host 先读单字节状态（`0`=成功，`1`=拒绝）；`libtrusty` 使用 `SO_RCVTIMEO`；CA 测试拒绝非 `[0]` 状态 |
| T-17 | IPv4 分片耗尽内核内存 | 中 | 外部持续发送无法完成重组的不同分片流 | 重组器限制队列数量与总内存，超过高水位后淘汰最早队列到低水位，队列存活时间固定为 30 秒 |
| T-18 | 重叠 IPv4 分片混淆上层解析 | 中 | 同一重组 key 提交相互覆盖的 payload range | 被已有区间完全覆盖的分片按重复包丢弃，任何部分重叠或总长度矛盾会删除整条队列 |
| T-19 | UDP 接收洪泛占满 socket 队列 | 中 | 应用读取速度低于入包速度 | 每个 PCB 最多保留 1024 个数据报；创建时预留指针大小的队列槽位，达到上限时在锁外丢弃新数据报；PCB 直接复用报文进入网络栈时创建的 `PacketBuf` 句柄 |
| T-20 | pathname Unix socket 绕过 inode/目录 DAC 或复用已有 inode | 高 | bind/connect/sendto 直接访问 binding 表，或 bind 接受已有路径 | bind 通过 `parent_at` 和 `Path::mknod` 排他创建；connect/sendto 在 lookup 后检查最终 inode `MAY_WRITE`；abstract 地址才直接访问内存 binding 表 |
| T-21 | 内核任务隐式读取用户凭据 | 高 | 启动期 pathname bind 调用普通 `SocketOps::bind`，当前线程不存在或主体错误 | 内核调用者使用 `bind_with_cred` 显式传入 `initial_cred()` 等已选择凭据；普通入口只服务当前用户任务 |
| T-22 | Unix stream 在 EOF 后发布数据 | 中 | send、shutdown 与 peer recv 并发交错，关闭状态和 write index 缺少共同排序 | 每个发送方向使用共享 `tx_order`；send 在锁内复检后发布，recv 在锁内复查 empty 和 closed，Channel 释放前先发布关闭状态 |
| T-23 | netlink socket 复用旧凭据导致越权 mutation | 高 | socket 跨进程传递或调用者凭据变化后继续使用创建时权限 | POSIX send 与 socket file write 路径仅在 netlink 分支取得当前 `Cred`，`NetlinkSocket::send_with_cred` 逐条检查 mutation；无权限请求生成 `NLMSG_ERROR` 和 `EPERM` |
| T-24 | 混合 netlink 批次发生部分 mutation | 高 | 同一发送同时包含 query 和 mutation，framing 错误位于已处理消息之后，或 response queue 在批次中途耗尽 | 发送事务锁串行化同一 socket 的 mutation 批次；完整批次先校验 framing 和类别；混合批次返回 syscall `EOPNOTSUPP`；同类 mutation 执行前检查完整 response 空间 |
| T-25 | poller 执行期间丢失新事件 | 中 | RX、TX 或 timer 通知与完成 CAS 并发 | `kwork::BudgetedPoller` 将执行期通知发布为 `RUNNING_PENDING`；完成 CAS 失败后按观察到的状态重试，并通过一次成功 CAS 同时释放执行权和发布下一轮 |
| T-26 | smoltcp 协议 timer 缺少推进事件 | 中 | TCP 重传或 keep-alive deadline 到期时没有设备 IRQ 或 socket 调用 | 每次 Interface poll 后通过 `poll_at` 注册 timer；到期后调用 `notify(PollReason::Timer)` 并唤醒 socket waiter；未来 deadline 不触发立即重轮询 |
| T-27 | socket TX 工作缺少后台推进 | 中 | connect、send、receive window update 或 close 只更新 socket 状态 | 真实 TX 生产路径调用 `notify(PollReason::Tx)`；TCP 与 raw 注册 smoltcp send waker；Router data TX queue 容量增加后唤醒全局 TX waiter |
| T-28 | TCP 文件关闭丢失已经接受的发送数据 | 中 | `write` 把数据写入 smoltcp buffer 后异步发布 TX，文件 Drop 在 poller 处理前删除 handle | Drop 将未完成协议关闭的 handle 转移到 deferred-close registry；poller 完成 payload、FIN 和重传后回收；60 秒期限只限制 orphan `FIN_WAIT_2` 占用资源 |
| T-29 | 并发 socket 等待阻塞网络 timer 更新 | 中 | socket 注册等待时在 timeout mutex 内手工 poll 异步 sleep，timer-wheel 锁等待阻塞 poller | deadline 使用原子值发布；周期采样回调到期后直接唤醒 waiter 并通知 poller，不持有 timeout mutex，不创建或 poll timer future |
| T-30 | TCP close 将未读数据错误转换为 FIN | 中 | 文件对象释放前未检查接收队列，协议 close 直接进入 FIN_WAIT1 | 对端收到 FIN 后继续按有序关闭处理，Linux 预期的 RST 语义丢失 | Drop 检查 smoltcp 接收队列；存在未读数据时调用 abort，并保留 handle 直到 RST dispatch 完成 |
| T-31 | TCP 零窗口恢复缺少后台推进 | 中 | 应用在接收窗口仍编码为零时读取 TCP 接收缓冲区，却没有发布窗口恢复工作 | 接收缓冲区余量低于最大窗口缩放量子时，读取操作调用 `notify(PollReason::RxWindow)`；达到缩放量子后的窗口增长由 RX 和已登记的协议 timer 推进 |
| T-32 | poller 执行期间丢失新事件 | 中 | RX、TX 或 timer 通知与完成 CAS 并发 | NetRx softirq、TX 和 timer 通知复用同一个 `kwork::BudgetedPoller` 状态机；执行期工作发布为 `RUNNING_PENDING`，完成 CAS 失败后按观察到的状态重试，并通过一次成功 CAS 同时释放执行权和发布下一轮 |
| T-33 | link 复合更新发生部分提交 | 中 | 重命名成功后 MTU 或 flags 校验失败 | `Router::update_device_link` 在写入前校验目标设备、名称格式与唯一性和 MTU 范围；rtnetlink 入口忽略 `IFF_VOLATILE` 变更并拒绝实际改变的未支持可写位，设备 setter 只接收已验证值 |
| T-34 | poller 推进权被普通任务持有的共享 mutex 长时间占用 | 中 | poller 取得 `RUNNING` 后等待被其他 socket 或控制面任务持有的 Router、IngressProcessor、Interface 或 socket-set | poll round 对这些共享锁使用 `try_lock`；竞争时保留待交接 batch，返回 `has_more` 并沿原四态状态机安排后续轮次；assist 仍限一轮，获取推进权采用机会式 CAS |
| T-35 | loopback UDP 依赖 poller 所有权才能完成投递 | 中 | `sendto` 只把报文排进 TX queue，短 `SO_RCVTIMEO` 在 poller 被占用时到期 | loopback xmit 在关闭 BH 前盖戳并入 `NET_RX_QUEUE`，raise `NetRx`；BH 恢复时把完整 IPv4 UDP 写入 PCB；TCP/ICMP 与未命中 socket 的 UDP 仍走 poller |

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
| F-07 | poll waker 或协议 timer 丢失 | device mask 错误、timeout 或 Ethernet RX poll source 未注册、注册所有者未跨 `Pending` 存活、缺少 register 后复查或 timer 到期后未通知 poller | socket 阻塞等待延迟，TCP 重传或 keep-alive 延后 | 应用 IO latency 上升或连接超时 | 3 | bind/connect 后更新 device mask；`Service::register_rx_waker` 使用同一个 `PollContext` 注册 timeout poll 和支持 interrupt-driven RX 的 Ethernet RX poll source；未 attach `NetRxScheduler` 的设备回退 timeout 聚合 waker；调用方持有 `PollRegistrations` 并在注册后复查 readiness；每次 Interface poll 后按 `poll_at` deadline 注册 timer，到期后调用 `notify(PollReason::Timer)` |
| F-08 | malformed netlink request | header 或 attr 长度非法 | 返回 empty response 或 netlink error | 调用者请求失败 | 4 | checked reader 和 error response 处理 |
| F-09 | 网络服务或控制面未初始化 | AF_PACKET 或 netlink route 请求早于 `SERVICE` 或 Router 初始化 | AF_PACKET 设备绑定与发送返回 `ENODEV`；link dump 为空，mutation 返回错误，地址或路由 dump 为空 | 对应网络功能暂不可用 | 2 | 所有 `SERVICE` 查询先检查初始化状态；初始化路径在公开网络接口可用前完成 Router 与 `SERVICE` 初始化；新增启动路径需保持顺序 |
| F-10 | Unix stream peer 提前关闭 | channel 被 shutdown 或 drop | send 在无发送进度时返回 `BrokenPipe`，已有进度时返回部分字节数；recv 排空本端缓冲后返回 EOF；peer 关闭时丢弃未读输入则返回一次 `ConnectionReset`；poll 报告 `RDHUP`、`HUP` 或 `ERR` | 应用感知连接关闭 | 4 | endpoint atomic 记录双方半关闭状态与待处理 reset；per-direction `tx_order` 统一关闭与数据顺序；三组 `PollSet` 按受影响事件定向唤醒 |
| F-11 | 中断上下文执行重型网络推进 | IRQ 或 `NetRx` 误调用 socket send、recv 或 `poll_interfaces` | 锁竞争、调度延迟或死锁 | 网络 IO 延迟上升，严重时系统卡顿 | 2 | IRQ 路径只做设备 ack、RX pending 标记和 `NetRx` 调度；Ethernet `NetRx` 只唤醒设备 RX `PollSet`；loopback 在关闭 BH 前给已有 `PacketBuf` 盖戳，`NetRx` 从 `NET_RX_QUEUE` 只取出盖戳 UDP 并通过无分配的 `SpinNoIrq` lookup 与句柄入队路径交付，不查找 `LoopbackDevice`；不得从 softirq 调用 `poll_interfaces` 或获取 `Service` / `SocketSet` mutex |
| F-12 | RX buffer recycle 顺序错误 | frame payload 在 `recycle_rx` 后仍被引用 | 读取悬垂数据或数据损坏 | packet 解析异常，严重时破坏内存安全 | 1 | `EthernetDevice::poll_rx` 在 recycle 前完成解析和复制；新增设备适配需保持同样生命周期 |
| F-13 | port 0 handshake 永久等待 | host 早于 TA publish 连接且未设 recv 超时；或 service 永不 publish | host `read` 阻塞；负例测试挂起 | CA/测试进程无响应 | 3 | dynamic connect 保留 `WAIT_FOR_PORT`；host 设 `TRUSTY_VSOCK_TIMEOUT_SEC`；明确拒绝场景回 `[1]` |
| F-14 | 快速重连 `tipc_connect` 超时 `-11` | `route_event` 在 `has_connection()` 前丢弃同批 `Received`，service-name record 丢失 | host status-byte `EAGAIN`；约半数快速重连失败 | storage client/proxy harness 间歇失败 | 2 | mapped bridge port 仅按 `local_port` 认领；事件入 FIFO，不依赖 `has_connection()` |
| F-15 | IPv4 分片重组超时 | 首片到达后 30 秒内缺少后续分片 | 当前数据报丢失 | UDP 接收超时 | 3 | 删除过期队列；首片存在且允许回复时发送 ICMPv4 Fragment Reassembly Timeout |
| F-16 | UDP DF 数据报超过路由 MTU | `IP_MTU_DISCOVER` 要求 DF 且 packet 长度超过路由 MTU | 当前发送失败 | 应用收到 `EMSGSIZE` | 4 | 发送前读取路由 MTU，Router 拒绝对 DF 包执行输出分片 |
| F-17 | 启动期 Unix pathname bind panic | 内核任务调用隐式 `current_cred()`，但尚无当前用户线程 | `/dev/log` 等内核 socket 无法绑定 | 启动中断 | 2 | 启动期调用 `bind_with_cred` 并显式传入 `initial_cred()`；保留可用的初始 fs context |
| F-18 | smoltcp 过期 poll 期限变成超长等待 | 有符号微秒差值为负后通过 `as u64` 转换 | soft timer 被设置到远未来 | TCP 数据路径停顿，可能伴随 timer IRQ 异常 | 2 | 在同一 epoch 下直接把 `SmoltcpInstant` 映射为 `MonotonicInstant`，不计算无符号 delay；单测覆盖过期和未来期限 |
| F-19 | 无权限 netlink mutation | 当前发送凭据不具备配置权限 | mutation 不执行，RX queue 收到 `NLMSG_ERROR` 和 `EPERM` | 调用者配置失败 | 4 | 每次发送重新检查凭据；error 入队后发送入口返回已消费请求长度 |
| F-20 | 混合或畸形 netlink 批次 | 单次发送混合 query 与 mutation，或后续 message 的长度和对齐非法 | 整批未执行并返回 syscall 错误 | 调用者需修正或拆分批次 | 4 | 批次分类和 framing 校验在状态更新及 response 生成前完成 |
| F-21 | RX 或 TX backlog 长期占用推进任务 | 持续高包率超过单轮 budget | 单轮达到预算或 1 ms 软时间上限并留下 backlog | 其他任务调度延迟或网络吞吐下降 | 3 | 设备 RX、stack ingress、stack egress 和 Router TX 最多处理 32 个工作项后检查时间；每批最多连续执行四轮，达到上限后重新唤醒并归还执行权，剩余 backlog 进入下一批 |
| F-22 | TCP peer 在本地文件关闭后不发送 FIN | peer 已确认本地 FIN，协议 handle 长期停留在 orphan `FIN_WAIT_2` | deferred-close registry 持续持有 socket buffer | 网络内存随失联连接增长 | 3 | 进入 `FIN_WAIT_2` 时设置 60 秒期限，期限进入统一 poll timer，到期后回收 handle |
| F-23 | IPv4 输出设备管理 down | `RTM_NEWLINK` 清除匹配路由设备的 `IFF_UP` | 当前 UDP 或 raw IP 发送失败 | 应用收到 `ENETUNREACH` | 4 | UDP 和 raw IP 在提交发送缓冲前通过 `SERVICE` 校验输出路由和设备管理状态 |
| F-24 | 数据面共享锁竞争 | socket 或控制面任务持有 Router、IngressProcessor、Interface 或 socket-set | 当前 poll round 提前结束并保留 batch | 网络工作延后到下一轮 | 3 | 返回 `has_more`，单执行者完成 CAS 发布 `SCHEDULED`；raw、accepted 和 control batch 保持所有权，下一轮从保留位置继续 |
| F-25 | 短超时 loopback UDP 在 poller 忙碌时丢包 | `sendto` 只通知 poller，`recvfrom` 的 1 jiffy 超时先到期 | 并发 libc-test `socket.exe` 间歇 `ETIMEDOUT` | 用户可见功能回归 | 3 | loopback UDP 在发送路径的 BH 窗口内由 `NetRx` 交付 PCB；poller 不再承担该交付；压力测试覆盖 SMP=1 与 SMP=4 的四并发短超时场景 |
| F-26 | TCP child 永久滞留 SYN 队列 | packet mark 在 child 进入可接受状态前被消费，后续 ingress 或 timer poll 没有刷新 listener | accept 队列保持为空 | 服务端连接建立后仍阻塞于 `accept` | 2 | SYN 队列非空的 listener 在每次实际网络 poll 后刷新；仅在 child 移入 accept 队列后唤醒 waiter；SYN 队列不请求连续 poll |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- 普通输入错误使用 `KError` 和 `LinuxError` 返回，例如 `EINVAL`、`EAFNOSUPPORT`、`ENETUNREACH`、`EADDRINUSE`、`EWOULDBLOCK`。
- netlink framing 错误在处理前返回 syscall `EINVAL` 且不生成 response；完成批次拆分后的 payload 或 attribute 错误返回 `NLMSG_ERROR`。
- 无权限 netlink mutation 在 RX queue 中返回带 `EPERM` 的 `NLMSG_ERROR`；混合查询和 mutation 批次在修改状态前返回 syscall `EOPNOTSUPP`。
- malformed Ethernet、ARP、IP、UDP、TCP 包在 RX 路径丢弃，并通过 warn 或 trace 记录。
- UDP PCB 接收队列和 Router TX 队列满时映射为丢包或 `WouldBlock`，poller 负责等待 IO readiness。
- smoltcp buffer 和 Unix stream ring buffer 满时映射为 `WouldBlock`，poller 负责等待 IO readiness；非阻塞 Unix stream send 已有进度时返回部分字节数。
- loopback 和 Ethernet 队列满时丢包并记录 warn。共享 `NetRx` 队列满时 loopback xmit 仍对发送路径返回成功，对齐 Linux `loopback_xmit` 在 `__netif_rx` 返回 `NET_RX_DROP` 时仍给出 `NETDEV_TX_OK`。
- panic 路径主要来自初始化顺序、内部 invariant 破坏和 `expect` 断言；新增公开入口应先返回 `KError`，再进入内部断言区。

## 隐私分析

`knet` 会处理用户进程通过 socket 发送的 payload、从网络收到的 packet payload、Unix socket credentials、netlink 消息和 vsock payload。
这些数据在内核内按 socket buffer、ring buffer、driver buffer 或 netlink queue 保存。
模块自身不做持久化，也不把 payload 写入日志；trace 日志当前会输出 Ethernet frame 字节，生产环境需按日志级别控制敏感网络数据泄露。

## 已知限制

- `RTM_GETNEIGH` dump 列入后续范围，neighbor 通过 `RTM_NEWNEIGH` mutation 进入设备 owner。
- IPv4 输出使用匹配路由指向设备的 MTU，ICMP Fragmentation Needed 中的 next-hop MTU 只进入 UDP error queue，尚未形成动态 PMTU cache。
- IPv4 输出分片只支持无 options 的栈内生成报文，尚未实现 options copy 语义。
- route dump 读取 Router 中的配置路由和地址派生路由。
- smoltcp maintenance 与单次 egress pass 提供有界执行，ingress 通过 `poll_ingress_single` 按 packet 推进；单个 packet 的协议处理和一次 egress pass 仍会形成有限的软上限超出。
- Router、IngressProcessor、Interface 和 socket-set 的竞争通过非阻塞退让保持 poller 所有权边界；transport、listen table 与设备回调内部的短临界区仍会形成有限的软上限超出。
 - UDP RX queue 在 PCB 创建时一次保留 1024 个指针大小的槽位，空闲 socket 仍承担这部分固定元数据开销。
 - address 与 route dump 直接读取 Router，link dump 直接读取设备实时快照。
- raw socket 创建权限由 syscall 层承担，`knet` 构造器自身没有进程凭据参数。
- Ethernet 设备只处理 IPv4 ARP，IPv6 NDP、非 Ethernet 链路和多队列 NIC 抽象仍待扩展。
- crate 内 UDP 数据路径当前只支持 IPv4；IPv6 UDP 继续由 smoltcp DNS 路径使用，普通 UDP socket 不提供 IPv6 收发。
- vsock-TIPC bridge v1 不转发 TIPC handles 或 memrefs，也不为 vsock send credit 建立持久重试队列。

## 审计清单

修改本模块时需验证：

- 每个 `unsafe` 块均有 `SAFETY:` 注释。
- 新增 smoltcp socket handle 的生命周期受 `SOCKET_SET` 保护。
- 新增 link 与 neighbor mutation 直接更新目标设备 owner；address 与 route mutation 在 `rtnl_lock` 内更新 Router 并刷新派生投影；最后一个同值地址删除时同步清理 Router 中依赖该 `prefsrc` 的配置路由；设备移除走 `unregister_netdev`，在同一把锁下删除并重编号路由与设备邻居。
- 每次 netlink `send` 或 `write` 传入当前调用者的 `Cred` 快照，socket 不缓存权限。
- 新增 netlink parser 先校验单条 header 长度和批次对齐边界，再校验 attribute、family 和 index。
- 同一 netlink socket 的发送事务保持串行，跨 socket mutation 由 `rtnl_lock` 串行化；设备移除走 `unregister_netdev`；锁顺序为发送事务锁、`rtnl_lock`、控制面 owner 锁、rx queue。
- 仅含查询或仅含 mutation 的批次支持逐条处理，混合批次在状态更新前返回 `EOPNOTSUPP`，mutation 批次执行前检查完整 response 空间。
- 新增 ring buffer 操作的 advance count 来自同一锁内同一批 slices。
- Unix stream 的 write index 发布、方向关闭和空队列 EOF 判定保持同一个 per-direction 排序点，方向锁内不执行用户复制或 `PollSet` 唤醒，shutdown 在释放 `channel` mutex 后执行 waiter 唤醒。
- 新增外部网络输入使用 checked parser。
- IPv4 分片重组改动保持队列数量、内存和超时上限，并拒绝重叠 range。
- UDP registry 改动保持 bind、connect、普通接收和 ICMP error lookup 使用同一 PCB 所有权来源。`NetRx` 与 task 共享的 bucket、connected peer 和 PCB 接收队列必须使用 `SpinNoIrq`，sleepable 状态更新不得放在这些锁内。
- 修改 poller 状态机或 `Service::poll_budgeted` 时验证四态转换、单执行者获取、一次 CAS 释放、分阶段 smoltcp 预算和 1 ms 软时间上限；`PollProgress::has_more` 只能包含立即工作，未来协议 deadline 必须等待 timer 到期后发布；TX waiter 只能由 `tx_capacity_changed` 触发全局容量唤醒。
- 修改 `Service::poll_budgeted` 的锁路径时验证共享推进锁保持 `try_lock` 语义，竞争时设置 `has_more`，raw、accepted 和 control batch 在重试前保持所有权，TCP listener preparation 只在 accepted batch 能立即转入 Router 时执行。
- 修改 TCP listener 推进时验证 SYN 队列非空的 entry 在实际网络 poll 后得到刷新，child 移入 accept 队列后才在 entry 锁外唤醒 waiter，并保持 SYN 队列不进入 `PollProgress::has_more`。
- 修改 poller 执行边界时验证 assist 保持一次机会式 CAS 和单轮预算；loopback UDP 必须在发送路径的 BH/`NetRx` 窗口内进入 PCB，不依赖 poller 所有权。
- 修改 `NET_RX_QUEUE` 时验证 `pending_udp` 与 `deferred` 在设备创建时完成预分配、`enqueue` 上限为两条队列长度与 in-flight 合计、`SpinNoIrq` 内不发生扩容、堆释放或 UDP PCB 交付，队列满与 `discard_ifindex` 都在锁外 drop `PacketBuf`，`process_pending` 批次不超过 `NET_RX_BUDGET`，以及发送路径 fallback 只执行固定轮次；共享该队列的单元测试必须声明 `serial`。
- 修改 UDP PCB 队列时验证 PCB 创建时预留 1024 个指针大小的槽位、`PacketBuf` 在进入网络栈时建立引用计数生命周期、loopback 在关闭 BH 前写入已校验 UDP 元数据、softirq 与 `SpinNoIrq` 入队路径不分配或扩容、满队列报文在锁外释放、FIFO 顺序，以及 `MSG_PEEK` 的 payload 复制位于锁外。
- 修改 TCP Drop 或异步 TX 路径时验证已成功写入的 payload 在文件关闭后仍由协议对象持有，未读数据关闭发送 RST，deferred-close handle 在 `Closed` 后删除，状态期限只作用于 orphan `FIN_WAIT_2`。
- 新增 socket option 明确 errno、阻塞语义和 poll readiness。
- 新增 pathname Unix socket 入口使用单次凭据快照，并让全部 VFS 操作显式接收该快照。
- pathname bind 保持排他创建、`0777 & !umask` mode 和 fs credential owner；connect/sendto
  在读取 binding 前检查最终 inode `MAY_WRITE`。
- 新增内核调用路径不得依赖 `current_cred()`；调用者必须显式选择凭据。
- 新增公开 API 先确认是否需要跨 crate 暴露。
