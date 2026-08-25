# knet — 设计文档

## 定位

`knet` 提供 x-kernel 的内核网络栈抽象。
它把驱动层网卡、smoltcp 协议栈、socket 对象、轮询唤醒、Unix domain socket、netlink 和可选 vsock 组合成统一的 `SocketOps` 接口。
上层 `posix/net` 负责 syscall 参数解析和文件描述符接入，`core/kruntime` 负责启动期设备注入，`knet` 负责协议状态、路由、收发推进和 socket 语义。

## 背景

x-kernel 运行在 `no_std` 内核环境中，无法直接使用 Linux 内核网络栈或标准库网络类型。
当前实现由 crate 内 IPv4、UDP 和 ICMPv4 数据路径处理 UDP 收发、分片与差错报告，smoltcp 继续处理 TCP、raw IP、IPv6 和 DNS，并提供兼容期 interface poll 模型。

## 范围

涉及的源文件：

```text
net/knet/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── poller.rs
    ├── device/
    │   ├── mod.rs
    │   ├── ethernet.rs
    │   ├── loopback.rs
    │   ├── net_rx.rs
    │   └── vsock.rs
    ├── link/
    │   ├── buf.rs
    │   ├── mod.rs
    │   ├── packet.rs
    │   └── wire.rs
    ├── netlink/
    │   ├── mod.rs
    │   ├── rtnetlink.rs
    │   ├── socket.rs
    │   └── wire.rs
    ├── socket/
    │   ├── mod.rs
    │   ├── file.rs
    │   ├── general.rs
    │   ├── options.rs
    │   └── state.rs
    ├── stack/
    │   ├── fragment.rs
    │   ├── ingress.rs
    │   ├── ipv4.rs
    │   ├── mod.rs
    │   ├── service.rs
    │   ├── router.rs
    │   ├── listen_table.rs
    │   └── wrapper.rs
    ├── transport/
    │   ├── mod.rs
    │   ├── tcp.rs
    │   ├── udp/
    │   │   ├── input.rs
    │   │   ├── mod.rs
    │   │   ├── output.rs
    │   │   ├── pcb.rs
    │   │   ├── registry.rs
    │   │   ├── state.rs
    │   │   ├── socket.rs
    │   │   └── wait.rs
    │   ├── raw.rs
    │   └── udp_err.rs
    ├── unix/
    │   ├── dgram.rs
    │   └── stream.rs
    └── vsock/
        ├── connection_manager.rs
        └── stream.rs
```

测试辅助代码位于 `socket/test_options.rs`、`socket/test_state.rs`、`netlink/tests.rs`，
IPv4、分片重组和 UDP 的回归测试与对应实现放在同一模块内。

## 架构

```text
core/kruntime
   │ init_network
   v
┌──────────────────────────┐
│ Service                  │
│  ├─ smoltcp Interface    │
│  ├─ Router               │
│  ├─ IngressProcessor     │
│  └─ reusable batches     │
└──────────┬───────────────┘
           │
           v
┌──────────────────────────┐
│ Router                   │
│  ├─ RouteTable           │
│  ├─ LoopbackDevice       │
│  └─ EthernetDevice       │
└──────┬──────────────┬────┘
       │ UDP / ICMPv4 │ TCP / raw / IPv6 / DNS
       v              v
┌──────────────────┐  ┌──────────────────────────┐
│ UDP PCB registry │  │ SocketSetWrapper         │
│ and socket queues│  │  └─ smoltcp SocketSet    │
└────────┬─────────┘  └──────────┬───────────────┘
         │                       │
         └───────────┬───────────┘
                     v
┌──────────────────────────┐
│ SocketOps implementations│
│ TCP / UDP / raw / Unix   │
│ netlink / vsock          │
└──────────────────────────┘
```

| 组件 | 职责 |
|------|------|
| `init_network` | 创建 loopback 与首个 Ethernet 设备，建立默认路由，初始化网络全局状态，并启动 kwork-backed 后台 poller |
| `poller` | 通过 `kwork::BudgetedPoller` 的四态状态机串行化执行者，使用 dynamic kwork queue 或 socket assist 预算推进 RX、smoltcp timer 和 TX |
| `Service` | 持有加锁的 smoltcp `Interface`、`Router`、`IngressProcessor`、可复用 batch、poll timeout 和 RX waker 注册入口 |
| `Router` | 管理设备列表、IPv4 地址、配置路由、地址派生路由、RX 设备游标、smoltcp ingress queue、control/data TX queue、IPv4 输出分片、next-hop 选择和 budget dispatch |
| `IngressProcessor` | 在 Router 锁外校验和重组 IPv4 输入，执行 crate 内 UDP 分流、UDP error 与 TCP listen snoop，并生成 ICMPv4 control packet |
| `NetDevice` | 抽象 loopback 和 Ethernet 设备后端，持有接口名、MTU、管理 up 状态、link snapshot 和设备邻居状态，通过 `PacketBuf` 转移报文所有权，并使用 crate 内地址类型和 `MonotonicInstant` 表达设备边界 |
| `PacketBuf` | 指针大小的引用计数报文句柄，保存报文数据、协议偏移、接口索引、包类型、校验状态、已校验传输层元数据和当前所有者；共享后的写操作通过写时复制保持各句柄状态独立 |
| `link::wire` | 使用 `zerocopy` 校验和构造 Ethernet 与 Ethernet/IPv4 ARP 头部 |
| `stack::ipv4` | 使用 `etherparse` 校验和构造 IPv4 与 ICMPv4 头部，并执行输出分片 |
| `stack::fragment` | 按源地址、目的地址、标识、协议和接口重组本地 IPv4 分片，并限制队列数量、内存和存活时间 |
| `transport::udp` | 维护 UDP PCB registry、接收队列、bind 与 connect 状态、校验和、socket option、异步错误和 IPv4 收发 |
| `SocketSetWrapper` | 串行化 smoltcp socket set 与 TCP deferred-close 元数据访问，并在新增 socket 时通知等待者 |
| `ListenTable` | 管理 TCP listen backlog、SYN 队列、accept 队列和 accept waker |
| `GeneralOptions` | 统一管理 nonblock、reuseaddr、超时和设备 mask |
| `SocketOps` | 上层 socket syscall 使用的统一操作接口 |
| `netlink` | 提供 AF_NETLINK socket、kobject uevent 和有限 rtnetlink 协议适配，查询各 owner 的实时快照并把 mutation 交给对应 owner |
| `unix` | 提供 Unix domain stream 与 datagram transport；pathname 地址通过 kvfs 查找或创建 socket inode |
| `vsock` | 在 `vsock` feature 下提供 virtio-vsock stream 支持 |

### Ethernet RX 调度边界

驱动向上提供 NIC 能力时仍通过 `kclass` / `driver_net` 发布 `NetDevice`。
`knet` 在创建 `EthernetDevice` 时为该设备创建独立的 RX `PollSet`，
并通过 `driver_net::NetRxScheduler` 把一个 IRQ-safe 调度能力 attach 给驱动。
NIC 硬中断 handler 只负责 ack 设备中断并调用 `schedule_rx()`；
`schedule_rx()` 标记对应设备有 RX work，并 raise `kirq::softirq::SoftirqVec::NetRx`。
当前 `NetRx` softirq action 对 Ethernet 仍是保守 fallback 形态：它只消费已 pending
的设备 RX source，唤醒对应设备 RX `PollSet`，并调度 `knet-poller` dynamic kwork
执行 sleepable 协议推进；普通 socket 轮询路径也可以在被唤醒后执行一次 assist。
Loopback 走单独的 Linux 对齐路径：xmit 在关闭 BH 前给完整 IPv4 UDP 盖上接收元数据，
把同一 `PacketBuf` 放入共享 `NET_RX_QUEUE` 的 `pending_udp` 并 raise `NetRx`，
`local_bh_enable` 在发送路径上跑 `NetRx`，已盖戳的 UDP 直接进入 PCB。
`NetRx` 只 drain `pending_udp`，不查找 `LoopbackDevice`。
TCP、ICMP、IPv6、分片和未命中 socket 的 UDP 留在 `deferred`，由任务 poller `poll_rx` 取出。
这与 Linux NAPI 的最终形态仍不同：Linux `NET_RX_SOFTIRQ` 会在
`net_rx_action()` 中直接按 budget drain NAPI poll list。X-Kernel 后续要
达到同类形态，需要先补齐 Ethernet 非阻塞 RX ingress 或 workerqueue 承接层，因为当前
`Service` / `SocketSet` 和 TCP listen table 仍使用可睡眠锁。
如果驱动不支持 `NetRxScheduler` attach，`EthernetDevice` 不会注册专用 RX
`PollSet`，等待路径回退到 `Service` 的 timeout 聚合 waker，避免等待一个
永远不会被驱动唤醒的 source。如果 `NetRx` softirq vector 无法注册，
`EthernetDevice` 同样不创建专用 RX source，并保留 timeout polling fallback；
这属于 fail-closed 行为，而不是创建一个不可达的异步等待源。

这个边界避免把网络 source 语义放进 `kirq`：
`kirq` 只提供通用 softirq 机制，`driver_net` 表达网络设备能力，
`knet` 决定 RX work 如何唤醒和推进。

## 调用约束 / 执行上下文

`knet` 运行在内核上下文中，
但并非所有路径都适用于任意执行环境。
调用者需要满足以下约束：

- **依赖初始化顺序**：在创建 socket、访问路由状态、
  或推进协议栈之前，必须先完成 `init_network`，
  以初始化 `SERVICE`、`SOCKET_SET`、`LISTEN_TABLE`
  和 Router 初始状态。`knet-poller` 的 dynamic workqueue 在这些对象完成初始化后启动。
- **普通协议推进不应在硬中断上下文执行**：
  `poll_interfaces`、socket send/recv/connect/accept、
  netlink mutation 等路径会获取 `Mutex` / `RwLock`
  并推进较重的数据路径，应在普通任务上下文中运行。
- **IRQ 路径只做通知**：
  设备中断回调应只负责 ack 设备中断、登记 RX pending 并调度
  `NetRx` softirq，不应直接执行完整的协议推进或阻塞式 socket 语义。
- **NetRx softirq 可交付 loopback UDP，但不得调用 `poll_interfaces`**：
  `NetRx` 运行在不可睡眠上下文。Ethernet 路径只消费 pending RX source
  并唤醒对应设备 RX `PollSet`，随后调度 `knet-poller` work 在 kwork 任务
  上下文执行受批次轮数上限约束的数据面推进。
  Loopback 发送侧在关闭 BH 前完成 UDP 校验，并把地址与 payload range
  写入现有 `PacketBuf` 的控制元数据；
  同一 action 只从 `pending_udp` 取出这些已盖戳句柄，经 `SpinNoIrq` PCB registry 移入已预留容量的
  接收队列并唤醒 socket waiter，因此 action 的 UDP 投递热路径不执行堆分配。
  TCP、ICMP、IPv6、分片和未命中 socket 的 UDP 留在 `deferred`，由任务 poller
  通过 `poll_rx` 取出。
  不得从 softirq 调用 `poll_interfaces` 或获取 `Service` / `SocketSet`
  的可睡眠锁。
- **允许阻塞的路径依赖 poll/waker 语义**：
  阻塞式 socket 操作依赖 `PollSet`、waker
  和 timeout 注册机制。`Pollable::register` 只能通过调用方提供的
  `PollContext` 注册源；调用方必须让对应 `PollRegistrations`
  跨 `Pending` 存活，并在注册后复查 readiness，以同时保证取消清理
  和关闭 check/register 竞态。
- **不要求固定当前进程线程才能访问全局状态**：
  `SERVICE`、`SOCKET_SET` 和 Router
  的共享访问主要依赖全局锁和原子状态，
  但 syscall 语义相关路径仍由 `posix/net`
  负责提供进程文件描述符与凭据语境。netlink `send` 和 socket file
  `write` 在操作入口取得当前调用者的凭据快照，socket 不缓存权限。
- **pathname Unix socket 需要文件系统与凭据语境**：
  用户态 `bind` / `connect` 必须在具有当前线程和 fs context 的任务中调用；
  不具有当前用户任务的内核调用者必须使用 `bind_with_cred`
  显式传入凭据，并保证调用环境具有可用的 fs context。
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

### rtnetlink 控制面

```text
RTM_NEWLINK ──> Service ──> Router ──> NetDevice link owner
                                             │
RTM_GETLINK <──────────── live LinkSnapshot ─┘

init_network ──> Router.ipv4_addrs ──> local / connected routes
                       │                    │
RTM_NEWADDR / DELADDR ┘                    ├─> smoltcp Interface / IngressProcessor
                       │                    └─> NetDevice address projections
RTM_GETADDR <──────── Router address snapshots
DELADDR 最后持有者 ────────────────────────> Router 删除失效 prefsrc 路由
设备移除 ── rtnl_lock ────────────> Service ──> Router 删除设备、地址、路由和邻居

init_network / RTM_NEWROUTE / RTM_DELROUTE ──> Router routes
RTM_NEWNEIGH ────────────────────────────────> NetDevice neighbor cache
RTM_GETROUTE <─────────────────────────────── Router route snapshot
RTM_GETNEIGH ────────────────────────────────> 后续范围
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| 设备 link 配置 | 更新后的设备 link 配置 | `RTM_NEWLINK` 经 `Service::update_device_link` 更新目标 `NetDevice` |
| 设备 link 配置 | `RTM_NEWLINK` dump | `RTM_GETLINK` 在 `SERVICE` 内读取每个设备的 `LinkSnapshot` |
| 初始 IPv4 地址条目 | `Router::ipv4_addrs` | `init_network` 通过 `Router::add_ipv4_addr` 注册，Router 同时生成 local 和 connected 路由 |
| IPv4 地址条目 | smoltcp、IngressProcessor 和设备地址投影 | 地址加入、删除或所属设备移除时由 `Service` 刷新所有派生视图 |
| 最后一个 IPv4 地址持有者 | `Router` 配置路由 | 地址删除时移除以该地址为 `prefsrc` 的配置路由 |
| 已移除设备 | `Router` 路由与设备邻居 | `unregister_netdev` 持有 `rtnl_lock`，Router 删除该接口的路由和邻居并重编号后续接口索引 |
| `RTM_GETADDR` | Router 地址快照 | rtnetlink 直接读取 `Service::ipv4_addr_snapshots` |
| 初始路由 | `Router` | `init_network` 直接调用 `Router::add_rule` |
| `RTM_NEWROUTE` 与 `RTM_DELROUTE` | `Router` | rtnetlink 在 Router owner 内完成校验和 mutation |
| `RTM_NEWNEIGH` | 目标 `NetDevice` | rtnetlink 通过 `Service` 和 `Router` 把更新交给设备邻居表 |

### NAPI 风格推进状态

```text
IDLE ──notify──────────────> SCHEDULED
  ^                              │
  │                              │ acquire
  │                              v
  └──finish without work────── RUNNING ──notify──> RUNNING_PENDING
                                 │                       │
                                 └──finish with work─────┴──finish──> SCHEDULED
```

| 当前状态 | 操作 | 下一状态 | 唤醒行为 |
|----------|------|----------|----------|
| `IDLE` | `notify` | `SCHEDULED` | queue `knet-poller` work |
| `SCHEDULED` | `notify` | `SCHEDULED` | 已有唤醒保持有效 |
| `SCHEDULED` | kwork callback 或 assist 获取执行权 | `RUNNING` | 当前执行者运行一轮或一批有界轮次 |
| `RUNNING` | `notify` | `RUNNING_PENDING` | 当前执行者完成时负责后续唤醒 |
| `RUNNING_PENDING` | `notify` | `RUNNING_PENDING` | pending 状态保持 |
| `RUNNING` | 完成且没有立即工作 | `IDLE` | 无唤醒 |
| `RUNNING` | 完成且仍有立即工作 | `SCHEDULED` | queue `knet-poller` work 进入下一轮 |
| `RUNNING_PENDING` | 完成 | `SCHEDULED` | queue `knet-poller` work 进入下一轮 |

`SCHEDULED` 表示已发布待推进工作，`RUNNING` 表示唯一执行者持有执行权，
`RUNNING_PENDING` 记录执行期间到达的新通知。完成路径通过一次 CAS 同时归还执行权
并发布 `IDLE` 或 `SCHEDULED`，通知与完成并发时由同一原子修改序列保留事件。
`SCHEDULED` 状态由 `knet-poller` dynamic kwork 或 socket assist 竞争批次执行权。
Loopback UDP 独立于 poller 执行权，发送路径在 BH 窗口内完成 xmit 与 `NetRx` 交付。
`Interface::poll_at` 返回的下一次协议 deadline 由 `Service` 注册为 timer，
到期后调度 poller 并唤醒正在等待 socket readiness 的任务。

## 算法流程

### 网络初始化

1. `init_network` 创建 `Router`。
2. 添加 `LoopbackDevice`，由 `Router` 注册 `127.0.0.1/8` 地址并生成 local、connected 路由。
3. 从 `DeviceContainer<NetDevice>` 取首个 NIC，包装成 `EthernetDevice`，注册默认 IPv4 路由。
4. 创建 `Service`，初始化内部 `Interface`、`Router`、`IngressProcessor` 和可复用 batch。
5. Router 直接持有初始路由，设备持有各自的邻居表；IPv4 地址投影由 Router 地址条目生成，link 初始状态由设备构造器建立。
6. 初始化全局 `SOCKET_SET`、`LISTEN_TABLE` 和 UDP PCB 及异步错误 registry。
7. 启动 `knet-poller` dynamic workqueue。
8. Ethernet 硬中断只 raise `NetRx` softirq；softirq 唤醒对应设备 RX `PollSet` 并调度 `knet-poller` work。

### RX 推进

1. `NetRx` softirq 在设备 RX pending 后调用 `NetworkPoller::notify(PollReason::Rx)`，调度 `knet-poller` work；socket recv 和 readiness 路径通过 `poll_interfaces` 尝试执行一次已有工作；TCP、UDP 和 raw TX 生产路径通过 `notify(PollReason::Tx)` 发布工作。TCP 从可编码为零窗口的低余量接收缓冲区消费数据时通过 `notify(PollReason::RxWindow)` 主动重开对端窗口。
2. 普通 `notify` 在 `IDLE` 上发布 `SCHEDULED` 并 queue `knet-poller` work。执行期间的新工作进入 `RUNNING_PENDING`；kwork callback 和 assist 只有一个能够通过 `SCHEDULED` 到 `RUNNING` 的 CAS。
3. `Service` 先按 TX budget dispatch 已排队的 control 和 data packet，使 loopback TCP/ICMP TX 在同一轮进入 `NET_RX_QUEUE`；本轮后续 TX dispatch 复用剩余预算。Loopback UDP 不依赖这一轮：`prepare_and_send_ipv4_packet` 对 loopback 目的地址直接 xmit，`send_ip_packet` 在关闭 BH 前盖戳并入 `NET_RX_QUEUE`，raise `NetRx` 后 BH 恢复时把完整 UDP 数据报送入 PCB。
4. `Router::drain_rx_budgeted_into` 按 RX budget 从设备轮转拉取 `PacketBuf`，`next_rx_device` 在设备间保持公平并随设备删除修正。
5. `IngressProcessor` 在 Router 和 smoltcp socket-set 锁外校验 IPv4 头、长度、校验和与本地目的地址，并完成 IPv4 分片重组。完整 UDP 数据报直接进入 crate 内 UDP PCB 分流，未命中 socket 的单播 UDP 触发 ICMPv4 Port Unreachable。
6. TCP、raw IP、ICMP 和 IPv6 packet 保存在可复用 accepted batch 中。按 smoltcp socket-set、Router 的顺序取得两把锁后，`prepare_smoltcp_ingress` 更新 TCP listener 状态并将 batch 转入有界 smoltcp ingress queue；只有 control batch 时跳过 socket-set。锁竞争期间保留 raw、accepted 和 control batch，当前轮返回 `has_more`，后续轮次继续交接。
7. `Service` 执行 `poll_maintenance`，按 RX budget 逐个调用 `poll_ingress_single`，并调用有界的 `poll_egress`。达到预算或时间边界后，剩余 ingress 保持 FIFO 顺序并进入后续轮次；`ListenTable::wake_touched_acceptors` 随后唤醒 accept 等待者。
8. `Router::dispatch_budgeted` 使用本轮剩余 TX budget 发送协议阶段新增的 packet，随后使用剩余 RX budget 再拉取一次设备队列。该尾部 RX 阶段让 loopback TCP/ICMP 完成 TX 与 smoltcp 交接，同时维持 assist 的单轮边界。
9. Router、IngressProcessor、smoltcp socket-set 或 Interface 的共享 mutex 发生竞争时，poller 通过 `try_lock` 直接退让。`Service` 返回带 `has_more` 的进度，唯一执行者按原状态机归还执行权并安排后续轮次。
10. `Interface::poll_at` 与 orphan `FIN_WAIT_2` deadline 共同决定下一次网络 deadline，`Service` 将其写入原子 deadline；`register_timer_callback(TIMER_SAMPLE_PERIOD)` 周期采样该值，到期后通过 `notify(PollReason::Timer)` 发布工作。尚未到期的 deadline 不进入 `PollProgress::has_more`。
11. `PollProgress::has_more` 只汇报当前可立即处理的 RX、ingress、TX、锁竞争重试或已到期 timer 工作。每轮 `Service::poll_budgeted` 使用 1 ms 软时间上限，并在设备 RX、stack ingress、stack egress 和 Router TX 每完成 32 个工作项后检查时间。协议 maintenance 和一次 smoltcp egress pass 始终执行，以维持 timer 与协议输出进度。kwork callback 或 assist 获得批次执行权后最多连续推进四轮，达到轮数上限或清空立即工作后归还执行权，剩余 backlog 通过 `SCHEDULED` 状态进入下一批。

### TX 路由

1. TCP connect、send 和 close，raw send 以及 UDP data queue 提交完成后发布真实 TX 通知；调用方可随后通过 `assist_once` 协助执行已调度的一轮。TCP recv 只在消费前的 smoltcp 接收缓冲区余量低于最大窗口缩放量子时发布 `RxWindow` 通知，覆盖未缩放零窗口和缩放后仍编码为零的窗口。达到缩放量子后的窗口增长随 RX 或已登记的协议 timer poll 推进，避免每次应用读取都创建后台工作。TCP 文件关闭不会立即删除仍处于协议关闭过程中的 handle。
2. crate 内 UDP 在 Router mutex 内选择处于管理 up 状态的输出设备、源地址和路由 MTU；路由缺失或输出设备管理 down 时返回 `ENETUNREACH`。写入 UDP 与 IPv4 头后，loopback 目的地址立即 `transmit_ipv4_now`；其余目的地址提交到 data TX queue。`transmit_ipv4_now` 忽略 `dispatch_ipv4_packet` 的 RX-ready 返回值：该 `bool` 是 poller 的 `poll_next` 提示，不是发送成败；共享 `NetRx` 队列满时丢包仍返回成功，对齐 Linux `loopback_xmit` 在 `NET_RX_DROP` 时返回 `NETDEV_TX_OK`。smoltcp 生成的 IP 包进入同一 data TX queue，`IngressProcessor` 生成的 ICMPv4 错误进入 control TX queue。
3. `IP_MTU_DISCOVER` 决定 UDP IPv4 头部的 DF 标志。超出路由 MTU 且允许分片的包由 `fragment_output_packet` 拆分，DF 包返回 `EMSGSIZE`。
4. `Router::dispatch_budgeted` 优先接管 control packet，再按 IP 版本解析源地址和目的地址；IPv4 输出先按实际长度更新 `total_len` 和头部校验和，并将已校验的源地址随报文传给 `NetDevice`。
5. 广播或组播包复制到所有设备；`0.0.0.0` 到 `255.255.255.255` 的受限广播允许在设备尚未分配 IPv4 地址时直接发送，用于 DHCP 初始化流量。
6. 单播包通过 `RouteTable::lookup` 选择最长前缀路由。
7. Ethernet 设备先查 ARP neighbor cache，命中后通过 `link::wire` 封装 Ethernet frame。
8. 未命中时发送 ARP request，并把 IP 包放入 `pending_tx` 等待 neighbor 解析。

当前实现保留 smoltcp TCP、raw socket 和 IPv6 推进。UDP、IPv4 分片重组、输出分片和 ICMPv4 UDP 差错报告使用 crate 内实现。
`PacketBuf`、`link::wire` 和 `stack::ipv4` 提供设备与 crate 内 IPv4/UDP 数据路径共用的报文表示和 parser/emitter。`PacketBuf` 对应 Linux `sk_buff` 的报文生命周期角色：报文进入网络栈时创建引用计数句柄，后续队列只移动或克隆该句柄；需要修改共享报文时执行写时复制。
路由表和设备接口使用 `crate::ip` 地址类型，设备时间使用 `ktime::MonotonicInstant`。
`Router`、`Service` 和初始化入口在 smoltcp 兼容边界完成地址与时间转换。

### TCP listen 和 accept

1. `TcpSocket::listen` 把本地 endpoint 注册到 `ListenTable`。
2. `IngressProcessor` snoop 首个 SYN 包，并调用 `ListenTable::incoming_tcp_packet`。
3. `ListenTable` 为新连接创建一个 smoltcp TCP socket，放入 SYN 队列。
4. 后续 poll 观察到连接可接受后移动到 accept 队列。
5. `TcpSocket::accept` 取出 `AcceptedTcp`，构造新的 connected `TcpSocket`。
6. POSIX 层通过 `AcceptOptions` 传入监听文件的 nonblocking 状态；accept 队列为空时，
   nonblocking 调用返回 `WouldBlock`，由用户态 poll/epoll 负责等待下一次可读事件。

### Unix pathname bind 和 connect

1. 用户态 `SocketOps::bind` 在操作入口获取一次当前线程的 `Arc<Cred>` 快照，并计算
   `0777 & !umask`，随后调用 `bind_with_cred`。
2. `bind_with_cred` 使用同一份 `&Cred` 和明确 mode，通过 `parent_at` 与 `Path::mknod`
   排他创建 socket inode；任意已有路径都映射为 `EADDRINUSE`，不会复用已有 socket inode。
3. kvfs 负责检查每一级目录的 search 权限、父目录的 write/search 权限，并以
   `fsuid` / `fsgid` 初始化新 inode 的属主。
4. 不具有当前用户任务的内核调用者显式传入凭据和 mode；例如 devfs 创建 `/dev/log`
   时传入 `initial_cred()` 与 `0755`，不尝试读取不存在的当前线程凭据。
5. pathname `connect` 和 datagram `sendto` 在操作入口获取凭据；kvfs 使用该快照完成
   全路径查找，并要求最终 socket inode 具有 `MAY_WRITE` 后才查询内存 binding。
6. abstract 地址不进入 VFS，因此不执行 pathname DAC 检查。

### rtnetlink 请求

1. POSIX send 路径和 socket file write 路径先区分协议，仅在 netlink 分支取得当前
   调用者的 `Cred` 快照，通过 `Socket::send_with_cred` 传给 `NetlinkSocket`。
2. `NlMsgHeader::read` 校验每条 netlink message 的 header 长度，批次拆分再校验
   message 对齐边界。framing 错误在处理任何 message 前返回 syscall `EINVAL`。
3. 仅含查询的批次逐条生成 response；仅含 mutation 的批次在检查完整批次的
   response queue 空间后逐条执行；混合查询和 mutation 的批次在修改状态前返回
   syscall `EOPNOTSUPP`。
4. 每条 mutation 使用本次发送携带的凭据检查权限。跨 socket mutation 与设备移除通过 `rtnl_lock` 串行化，地址存在性判断、地址更新和依赖的 Router 路由清理处于同一事务作用域。无权限请求生成带 `EPERM`
   的 `NLMSG_ERROR`，response 入队后发送入口返回已消费的请求长度。
5. `RTM_GETLINK` 从设备实时快照生成 multi-part response，`RTM_NEWLINK` 在整组名称、MTU 和 flags 校验通过后直接更新目标设备。
6. `RTM_GETADDR` 从 Router 地址快照生成 multi-part response；`RTM_NEWADDR` 和 `RTM_DELADDR` 直接调用 Router 地址 mutation，失败时返回 netlink error。重复地址策略在 Router 锁内判定；最后一个同值地址删除后，`RTM_DELADDR` 同时清理 Router 中以该地址为 `prefsrc` 的配置路由。
7. `RTM_GETROUTE` 和 route mutation 直接访问 Router，neighbor mutation 直接访问目标设备的邻居表，create/replace 判定与更新在同一 Router 临界区完成，带 `NLM_F_ACK` 时返回 ack。未识别的 route attribute 按 Linux `rtm_to_fib_config` 跳过，不返回 `EOPNOTSUPP`。

## 并发模型

- 全局 `SERVICE` 是 `LazyInit<Service>`。`Service` 内部分别使用 mutex 保护 smoltcp `Interface`、`Router`、`IngressProcessor` 和可复用 batch。poller 对前三类共享状态使用 `try_lock`，竞争时保留 batch 并返回立即工作；可复用 batch 只由取得全局推进权的执行者访问。网络 timer deadline 使用 `AtomicU64` 保存，周期采样回调只执行原子检查、waiter 唤醒和 poller 通知。
- `NetworkPoller` 通过 `kwork::BudgetedPoller` 保存 `IDLE`、`SCHEDULED`、`RUNNING` 和 `RUNNING_PENDING` 四态执行权。kwork 后台批次预算为 512 个 RX packet、256 个 TX packet 和 32 个 timer event，socket assist 预算为 16、16 和 8。每轮受 1 ms 软时间上限约束，每个批次最多连续执行四轮，每次 assist 最多执行一轮；达到批次上限后 queue 下一批并归还执行权。
- RX 设备拉取和 TX dispatch 位于 Router 锁内，IPv4 校验、过滤和 UDP 分流位于 Router 与 smoltcp socket-set 锁外。TCP snoop 在 accepted batch 已同时取得 Router 与 socket-set 后执行，避免 listener side effect 与 batch 交接分离。smoltcp ingress queue 有固定容量，`poll_ingress_single` 每次消费一个 packet，预算、时间边界或共享锁竞争留下的 packet 在后续轮次继续推进。
- 全局 `SOCKET_SET` 内部使用 `Mutex<SocketSetState>`，其中的 smoltcp socket set 与 TCP deferred-close 元数据共享同一所有权锁。关闭、登记、协议推进和回收不会跨两个 mutex 交接 handle。
- `UDP_PCB_REGISTRY` 按端口分为 256 个 bucket，每个 bucket 使用 `SpinNoIrq`，因为 `NetRx` 会在 softirq 中 lookup；每个 UDP PCB 的接收队列和 connected peer 同样是 `SpinNoIrq`，bind/connect 的 sleepable 状态更新放在 bucket 锁外。接收队列保存指针大小的 `PreparedUdpPacket`，其内部仍是报文进入网络栈时创建的 `PacketBuf` 句柄。数据报校验、地址与 payload range 解析发生在 loopback 发送侧进入 BH 前或普通 task ingress 中，结果写入同一报文的控制元数据；PCB 创建时按 1024 槽预留 `VecDeque` 容量，对齐 Linux `__udp_enqueue_schedule_skb` 在 `sk_receive_queue.lock` 下只 `__skb_queue_tail` 已有 `skb`。`enqueue` 在 softirq 与 `SpinNoIrq` 内只做占用检查和已有句柄拼接，满队列分支先释放队列锁，再回收当前报文；`MSG_PEEK` 只在锁内增加 `PacketBuf` 引用计数，payload 复制在 `SpinNoIrq` 外完成。共享 `NET_RX_QUEUE` 也使用 `SpinNoIrq`，由 loopback xmit、`NetRx` 和 task `poll_rx` 共用；`pending_udp` 与 `deferred` 都只保存指针大小的 `PacketBuf`，前者供 `NetRx` 按 budget 顺序取出盖戳 UDP，后者供任务 poller 取出其他报文。占用按两条队列长度与 in-flight 合计，避免在 PCB 交付期间被生产者填满腾出的槽。未命中 PCB 的 UDP 必须仍由该路径唯一持有句柄，清除元数据后放入 `deferred`。
- `Ipv4Reassembler` 位于 `Router` 内，并由 `SERVICE` mutex 串行访问。
- `LISTEN_TABLE` 用 `Mutex<HashMap<...>>` 管理 listener；每个 entry 的 `accept_poll`
  放在 entry 级 `Mutex` 之外，backlog 队列仍由该 `Mutex` 保护。`register_accept_waker`
  先无锁注册到 `accept_poll`，再短持锁做 readiness recheck，避免在队列锁内做
  `Waker::clone` / `PollSet` 工作，同时保留 register-recheck。
- TCP 状态转换使用 `StateLock` 的 atomic CAS，失败时返回当前状态。
- socket option 和 shutdown 标志使用 atomics，跨线程读写只表达配置或关闭状态。
- Unix stream 的 `channel: Mutex<Option<Channel>>` 串行化同一 endpoint 发起的 send、recv、shutdown 和 channel 释放。
- Unix stream 每个 endpoint 的 `tx_order: SpinNoPreempt<()>` 是对应发送方向的共享排序点。本端 write shutdown、对端 read shutdown、write index 发布和对端空队列 EOF 判定都经过该锁。固定锁序为 `channel` mutex 后取得单个 `tx_order`，两个方向的顺序锁不会同时持有。
- Unix stream 的用户数据复制和 `PollSet` 唤醒在 `tx_order` 外执行。send 先写入未发布的 vacant 区域，再在锁内复检关闭状态并推进 write index；recv 读取为空后在锁内复查 occupied 长度和关闭状态；shutdown 完成状态发布并复制 peer endpoint 引用后释放 `channel` mutex，再执行 waiter 唤醒。
- Unix stream 每个 endpoint 分别持有 readable、writable 和 connection-state 三组 `PollSet`。写入只唤醒 peer readable waiter，读取跨过发送缓冲低水位时只唤醒 peer writable waiter，半关闭只唤醒受影响方向，完整关闭通过 connection-state waiter 通知双方。
- Router 直接持有配置路由、地址派生路由和动态选源状态，设备直接持有邻居表，netlink 仅承担协议解析、实时快照编码和 owner 调用。`rtnl_lock` 串行化不同 socket 的控制面 mutation 和 `unregister_netdev`。每个 socket 的发送事务、rx queue 和 subscriber 列表使用独立 `Mutex`。发送事务锁串行化同一 socket 的容量预检、mutation 执行和 response 入队。response 在 rx queue 锁外生成，只在容量检查和入队时持锁；锁顺序固定为发送事务锁、`rtnl_lock`、Router、ingress、Interface、netlink rx queue，未涉及的锁按该序列跳过。
- RX waker 由 `GeneralOptions::device_mask` 指向相关设备。`Service::register_rx_waker` 使用调用方传入的同一个 `PollContext` 注册聚合 `timeout_poll` 和支持 interrupt-driven RX 的 Ethernet RX poll source；当前该 poll source 由 `NetRx` softirq 在设备 pending 后唤醒，普通 socket polling waiter 可以通过该 source 被唤醒，后台协议推进则由同一 softirq 调度的 `knet-poller` work 执行。registration 由跨越 `Pending` 的 `PollRegistrations` 统一管理。loopback 和未 attach `NetRxScheduler` 的 Ethernet 设备仍使用 `timeout_poll` 的聚合 waker，多任务广播由 `timeout_poll` 完成；设备层不得保存调用方的裸 waker或绕过 `Service` 注册互不等价的 task waker。
- 每次 `Interface::poll` 也会刷新独立的协议 timer，使 TCP 重传等 deadline 不依赖阻塞中的 socket waiter。

## 设计决策

### UDP 与 IPv4 数据路径迁出 smoltcp

UDP PCB、bind 冲突检查、收发队列、IPv4 校验、分片重组、输出分片和 ICMPv4 差错均由 crate 内类型管理。
TCP、raw IP、IPv6 和 DNS 继续通过 smoltcp socket set 推进，协议推进依赖 NetRx softirq 调度的 `knet-poller` work、TX/timer 后台 poller 调度和显式 `poll_interfaces` assist。
该边界让 UDP 不再依赖 smoltcp socket handle，同时保留现有 TCP 和 raw socket 行为。

### NAPI 风格预算推进

NetRx softirq、socket TX 生产路径、协议 timer 和 socket assist 共享同一个 `NetworkPoller`。`notify` 使用状态低位发布 pending 工作，`SCHEDULED` 到 `RUNNING` 的 CAS 提供单执行者约束，完成 CAS 同时归还执行权并发布下一状态。执行期间到达的通知把状态变为 `RUNNING_PENDING`，完成路径据此发布 `SCHEDULED` 并 queue `knet-poller` work。assist 在 `IDLE`、`RUNNING` 和 `RUNNING_PENDING` 上直接返回，不创建事件，也不等待执行权。

Loopback UDP 不经过 poller 所有权交接：发送路径对齐 Linux `dev_queue_xmit`（BH off）→ `loopback_xmit` → `__netif_rx` → `local_bh_enable` 跑 `NET_RX_SOFTIRQ`。完整 IPv4 UDP 在 BH 窗口内进入 PCB；TCP/ICMP/IPv6 仍由 poller 的 TX-then-RX 轮次交接。

RX、TX 与 timer 使用独立预算。`Service` 使用 smoltcp 分阶段 API 约束 stack ingress 和 egress，并使用 1 ms 软时间上限控制单轮占用。TX budget 在协议推进前后共享，RX budget 在主 RX 和尾部 RX 之间共享；该顺序让 loopback TCP/ICMP 在一次 assist 内完成设备往返。Router、IngressProcessor、Interface 和 socket-set 竞争会结束当前轮并设置 `has_more`，执行者继续沿四态状态机释放推进权。control TX 优先于 data TX，避免 ICMP 错误和协议控制流量长期滞后。TCP send 可写快路径向 smoltcp socket buffer 写入数据并发布 TX 通知，连续发送由 poller owner 在有界批次内完成协议推进和 TX dispatch。TCP recv 在消费前接收缓冲区余量低于最大窗口缩放量子时发布 `RxWindow` 通知，释放 socket-set mutex 后由 poller 推进窗口更新；达到缩放量子后的接收窗口增长复用 RX 和已登记的 timer 推进。TCP 和 raw socket 直接向 smoltcp 注册聚合 send waker；Router data TX queue 的可用 packet slot 实际增加时，`PollProgress::tx_capacity_changed` 触发 poller TX waiter 唤醒。协议与 deferred-close deadline 写入原子值，周期采样回调到期后唤醒 socket waiter，并以 `PollReason::Timer` 通知 poller。

### 控制面状态所有权

link 配置属于设备：接口名、MTU、管理 up 状态、operstate 和硬件地址通过 `NetDevice::link_snapshot` 统一导出，`RTM_GETLINK` 和 `RTM_NEWLINK` 直接访问该来源。AF_PACKET 热路径在单个 `SERVICE` 锁临界区内通过无分配的 `LinkSendSnapshot` 校验 up 状态、MTU 和硬件地址并发送，设备存在性检查使用 `has_device`，路由 MTU 查询直接读取匹配设备。设备配置或移除改变有效 MTU 时重建 smoltcp `Interface`，使缓存的 `DeviceCapabilities` 与设备集合一致；未改变有效 MTU 的配置更新保留当前 `Interface` 及其运行时缓存。

IPv4 地址条目由 `Router` 保存。每个条目包含设备索引、IPv4 CIDR 和 scope，加入条目时检查设备、单播地址和 smoltcp 地址容量。Router 为每个条目生成 `/32` local 路由，并按地址网络生成 connected 路由；地址删除同时移除派生路由、失效首选源路由和设备 pending TX。`Service` 将 Router 地址投影到 smoltcp `Interface`、`IngressProcessor` 和设备，Ethernet 根据设备地址处理 ARP、定向广播和待解析报文。Router 直接保存配置路由及运行时选择信息，Ethernet 设备直接保存邻居表，路由和邻居 mutation 均访问各自 owner。设备移除由 `unregister_netdev` 在 `rtnl_lock` 下交给 Router，统一清理路由、地址和设备邻居并重编号后续接口索引。

### pathname Unix socket 显式传递凭据

Linux 可从任务上下文隐式读取 `current_cred()`，但 knet 也服务于启动期内核调用者。
因此用户态入口只在操作开始时获取一次凭据快照，pathname VFS 路径继续显式接收 `&Cred`；
内核调用者通过 `bind_with_cred` 选择明确的凭据。
凭据不保存在 Unix socket、pathname lookup 状态或 dentry 中，避免凭据生命周期与路径状态耦合。
`sock_alloc_file` 也显式接收 `Arc<Cred>`，只把它交给 `VfsFile::f_cred`；socket 对象不保存
第二份 credential，也不在 knet 内部读取当前 task。

### rtnetlink 每次发送传递凭据

netlink socket 可以跨进程传递，创建 socket 时保存的凭据无法代表后续发送者。
POSIX send 和 socket file write 在各自入口区分协议，仅在 netlink 分支获取一次
当前凭据快照，随后通过 `Socket::send_with_cred` 显式传给 rtnetlink 权限检查。
普通协议继续使用 `SocketOps::send`，调用者凭据不会扩大到不依赖权限的传输实现。
没有当前用户任务的内核调用者通过 `send_with_cred` 显式选择凭据。

### 设备 mask 驱动 RX 唤醒

每个 socket 根据 bind 或 connect 结果记录设备 mask。
等待 RX 时只向相关设备注册 waker，同时注册 smoltcp poll timeout。
smoltcp `poll_at` 使用传入 timestamp 的同一 epoch 返回期限；兼容边界在
`SmoltcpInstant` 与 `MonotonicInstant` 之间直接映射时间点。过期的 smoltcp deadline
因此仍是过期的单调 deadline，不再计算有符号 delay，也不会经过 `as u64` 窄化。
这个设计减少无关设备中断唤醒，但依赖路由和地址同步保持 mask 准确。

### 后续 Ethernet NetRx direct progress 里程碑

Loopback IPv4 UDP 已经由 `NetRx` 直接交付 PCB。Ethernet 以及需要 smoltcp 的
协议仍不在 softirq 中推进。那部分应作为独立里程碑实现：

1. hardirq 仍只做设备 ack 和 `NetRxScheduler::schedule_rx()`；
2. `NetRx` softirq 先按 NAPI-like source state 认领 Ethernet RX work；
3. 若需要 sleepable 网络栈路径，softirq 只投递 workerqueue work，不直接调用
   `poll_interfaces`；
4. worker 或全链路 nonblocking ingress 负责按 budget 推进 `Router` /
   `Service` receive path，并在数据入 socket queue 后唤醒 socket waiter；
5. 该里程碑必须同时定义 RX buffer ownership、budget/repoll、cancel/flush 和
   device teardown 语义。

在该里程碑开始前，Ethernet `NetRx` 保持 per-device RX `PollSet` wake source
角色，同时通过 `knet-poller` work 承接 sleepable fallback progress；普通 socket
polling waiter 也可能是该 source 的消费者。

### TCP listen 表独立于 smoltcp listener socket

监听 socket 和 accepted child socket 分开管理。
`ListenTable` 根据收到的 SYN 创建 child smoltcp socket，把待完成连接放入 backlog 队列。
这个设计让 POSIX accept 语义集中在 knet 内部，代价是 listen table 必须 snoop TCP 首包并清理 aborted child。

### Unix stream 按 readiness 类别隔离 waiter

Unix stream 的双向 channel 为两端保存独立的 `StreamEndpoint`，endpoint 通过 atomic 记录本地读写关闭状态、监听状态和待处理连接错误，并通过 `StreamPollSets` 分别维护 readable、writable 和 connection-state waiter。listener 的连接请求复用 endpoint readable 集合。读取事件只注册到 readable，写入事件只注册到 writable，仅请求连接状态的 waiter 注册到 connection-state，避免同一方向重复占用多个固定容量集合。数据进入 ring buffer 时只通知对端读取者，接收端读取数据使发送缓冲占用量从容量四分之一以上降到四分之一以下时通知对端写入者。连接的接收和发送方向均关闭或 endpoint 被释放时同时通知三个集合，使所有受连接状态影响的等待者重新检查 readiness。

每个 `StreamEndpoint` 还保存本端发送方向的 `tx_order`。send 只在该锁内提交 write index，shutdown 只在对应方向锁内发布关闭状态，peer recv 只在同一锁内完成空队列和 EOF 的最终判定。该顺序保证已经发布的数据先于 EOF 被观察，关闭状态先取得方向锁时后续 send 返回 `BrokenPipe`。阻塞 send 已发布部分数据后遇到关闭时返回已发送字节数。非阻塞 send 在已有发送进度时返回部分字节数，零进展且 ring buffer 已满时返回 `WouldBlock`；连接存在且双方对应方向保持打开时，零长度发送返回 0。绑定只保留 Unix 地址，`listen` 单独开启连接接收；监听端关闭读取方向后拒绝新连接，并保留已入队连接供 `accept` 取出。

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
- TCP 文件对象 Drop 时先对 smoltcp socket 发起协议关闭。已经进入 `Closed` 且没有待发送复位报文的 handle 立即删除；接收队列存在未读数据时改用 abort 并保留 handle，直到 poller 发出 RST。其余 handle 转移到 deferred-close registry，继续参与 poller 推进、payload 发送、FIN、ACK 和重传。poller 在协议进入 `Closed` 后回收 handle。脱离文件对象引用的连接进入 `FIN_WAIT_2` 后启动 60 秒回收期限，并把该 deadline 合并到协议 poll timer；其他关闭状态不附加统一期限。
- TCP listener 关闭时，`ListenTable::unlisten` 标记 entry closed，drain child handles，并从 `SOCKET_SET` 删除。
- Unix stream listener 在 `Drop` 中清空 bind slot，释放 pending connection request。
- Unix stream channel 被 `Option<Channel>` 持有。shutdown 在对应方向锁内更新 endpoint 的读写关闭状态并唤醒受影响方向。`Channel::drop` 在释放 ring producer 和 consumer 前发布双向关闭状态并唤醒双方 connection-state waiter；peer 的 `recv` 先消费已发布数据，缓冲区耗尽后返回 EOF，`poll` 报告 `RDHUP`。关闭端丢弃自身接收队列中的未读数据时，对端记录一次 `ConnectionReset`，由 `recv` 或 `SO_ERROR` 消费。
- UDP socket Drop 时从 PCB registry 注销，PCB 销毁时释放接收队列与异步错误队列。
- Ethernet RX buffer 在 `poll_rx` 完成 frame 处理后调用 driver `recycle_rx` 归还。
