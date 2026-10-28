# knet — Design

## Test Layout

Guest tests live in `ut-tests/`, mirroring the source module hierarchy, and
use `unittest::path_mod_test!` at the original scope. Netlink and socket test
helpers move with their cases. Runtime timer probes retain their original
hang bounds and observe the boot-created service; migration does not install
another service or change its registration.

## Role

`knet` provides a kernel network stack for x-kernel, which combines driver-level network devices, protocol stack, socket objects, poll and wakeup mechanisms, Unix domain sockets, netlink, and optional vsock support into a unified `SocketOps` interface. It is mainly responsible for protocol state, routing, RX/TX progress, and socket semantics. It uses crate-local IPv4, UDP, and ICMPv4 data paths to handle UDP send and receive, fragmentation, and error reporting. smoltcp supplies TCP, raw IP, IPv6, and staged interface polling.

`NetDevice::poll_rx` returns `KResult<RxPoll>`. Ethernet consumes at most one
driver frame per call. `RxPoll::Empty` means the budgeted receive step consumed
no frame or deferred packet;
`RxPoll::Consumed` charges one unit of device RX budget and carries an optional
IP packet. ARP, malformed frames, and filtered or unsupported traffic consume
budget even when no packet reaches the IP stack. Receive and recycling failures
retain their mapped `KError`.
Loopback runs its stamped-UDP fallback with a separate entry-time round bound
before polling one deferred packet. The fallback is outside the Router's device
RX budget, so `RxPoll::Empty` can follow successful direct UDP delivery.
`RxDrain` reports the failing interface and error alongside any earlier packets
in the batch, and advances the device cursor. The service logs that error and
ends the RX round after handling the partial batch. A packet decoded during a
failed recycling operation is dropped; a later round may retry. The driver owns
the consumed buffer handle even on error.

## Background

x-kernel needs socket semantics that can serve both POSIX syscalls and in-kernel clients. `knet` owns protocol state, packet queues, routing, and progress scheduling, while its surrounding subsystems supply device access, task scheduling, filesystem operations, and caller context.

The POSIX network layer in `posix/net` handles syscall arguments, user-memory access, file descriptors, and ABI conversion. `kvfs` owns pathname resolution and access checks, and `anon_inodefs` creates socket files. `kclass` supplies registered network and vsock device handles; concrete drivers own hardware and virtqueue operations. `kirq` supplies the generic softirq mechanism, while `ktask`, `kworkng`, and `kpoll` supply timer callbacks, task-context polling, and readiness registration. `knet` determines how these facilities advance network work.

## Scope

The layout below shows the implementation and the kernel unit-test directory.
Tests in `ut-tests/` are loaded from their source modules with
`unittest::path_mod_test!` under `cfg(unittest)`.

```text
net/knet/
├── Cargo.toml
├── ut-tests/
└── src/
    ├── device/
    │   ├── ethernet.rs
    │   ├── id.rs
    │   ├── loopback.rs
    │   ├── mod.rs
    │   ├── net_rx.rs
    │   └── registry.rs
    ├── link/
    │   ├── buf.rs
    │   ├── mod.rs
    │   ├── packet.rs
    │   └── wire.rs
    ├── netfilter/
    │   ├── mod.rs
    │   └── filter/
    │       ├── mod.rs
    │       ├── matcher.rs
    │       ├── chain.rs
    │       ├── errors.rs
    │       ├── rule.rs
    │       └── table.rs
    ├── netlink/
    │   ├── mod.rs
    │   ├── rtnetlink.rs
    │   ├── socket.rs
    │   ├── tests.rs
    │   └── wire.rs
    ├── network/
    │   ├── ipv4/
    │   │   ├── mod.rs
    │   │   └── process.rs
    │   ├── interface_address.rs
    │   ├── fragment.rs
    │   ├── mod.rs
    │   └── router.rs
    ├── runtime/
    │   ├── mod.rs
    │   ├── poller.rs
    │   ├── service.rs
    │   └── tx_queue.rs
    ├── socket/
    │   ├── file.rs
    │   ├── general.rs
    │   ├── mod.rs
    │   ├── options.rs
    │   ├── state.rs
    │   ├── test_options.rs
    │   └── test_state.rs
    ├── stack/
    │   ├── ingress.rs
    │   ├── mod.rs
    │   ├── device.rs
    │   ├── router.rs
    │   ├── socket_set.rs
    │   └── time.rs
    ├── transport/
    │   ├── udp/
    │   │   ├── input.rs
    │   │   ├── mod.rs
    │   │   ├── output.rs
    │   │   ├── pcb.rs
    │   │   ├── registry.rs
    │   │   ├── relay.rs
    │   │   ├── socket.rs
    │   │   ├── state.rs
    │   │   └── wait.rs
    │   ├── listen_table.rs
    │   ├── mod.rs
    │   ├── raw.rs
    │   ├── tcp.rs
    │   └── udp_err.rs
    ├── unix/
    │   ├── stream/
    │   │   ├── channel.rs
    │   │   └── listener.rs
    │   ├── dgram.rs
    │   └── stream.rs
    ├── vsock/
    │   ├── bridge.rs
    │   ├── bridge_connection.rs
    │   ├── bridge_port_map.rs
    │   ├── connection_manager.rs
    │   └── stream.rs
    ├── consts.rs
    ├── control.rs
    ├── ip.rs
    ├── lib.rs
    ├── unix.rs
    └── vsock.rs
```

## Architecture

```mermaid
%%{init: {
  "theme": "base",
  "flowchart": {
    "htmlLabels": false,
    "curve": "basis",
    "nodeSpacing": 25,
    "rankSpacing": 45
  },
  "themeVariables": {
    "fontFamily": "\"Trebuchet MS\", Verdana, Arial, sans-serif",
    "fontSize": "15px",
    "primaryColor": "#F5F2EC",
    "primaryTextColor": "#493728",
    "primaryBorderColor": "#CFC8BD",
    "lineColor": "#9A9187",
    "clusterBkg": "#FAF9F6",
    "clusterBorder": "#D8D1C7",
    "edgeLabelBackground": "#FAF9F6",
    "textColor": "#493728",
    "nodeTextColor": "#493728",
    "clusterTextColor": "#493728"
  },
  "themeCSS": ".edgeLabel rect, .edgeLabel .labelBkg, .edgeLabel .background { fill: #FAF9F6 !important; stroke: none !important; } .edgeLabel, .edgeLabel p, .edgeLabel span { background-color: #FAF9F6 !important; color: #493728 !important; } .edgeLabel text, .cluster-label text { fill: #493728 !important; stroke: none !important; } .cluster rect { rx: 8px; ry: 8px; }"
}}%%
flowchart TB
    CALLER("posix/net · kernel clients")

    subgraph KNET["knet"]
        API("Socket API · SocketOps")

        subgraph SOCKETS["Socket implementations"]
            TCP("TCP / raw IP
smoltcp sockets")
            UDP("UDP
knet PCB / queues")
            PACKET("AF_PACKET
Ethernet frames")
            UNIX("Unix
Local IPC")
            NETLINK("netlink
uevents / rtnetlink")
            VSOCK("vsock · optional
Guest-host streams")
        end

        subgraph IP["IP processing"]
            SMOL("smoltcp
IPv4 / IPv6")
            IPV4("knet IPv4 / ICMPv4
Reassembly / UDP delivery
Error handling")
        end

        ROUTER("Router
Routes / TX fragmentation
RX-TX queues")

        subgraph DEVICE["Devices · NetDevice"]
            LOOP("LoopbackDevice")
            ETH("EthernetDevice
Ethernet / ARP")
        end
    end

    NIC("kclass network device
NIC driver")
    VIRTIO("kclass vsock device
virtio-vsock driver")

    CALLER --> API
    API --> TCP & UDP & PACKET & UNIX & NETLINK & VSOCK
    TCP --> SMOL
    UDP --> IPV4
    SMOL --> ROUTER
    IPV4 --> ROUTER
    PACKET -->|link TX| ROUTER
    NETLINK -.->|rtnetlink configuration| ROUTER
    ROUTER --> LOOP & ETH
    ETH --> NIC
    VSOCK --> VIRTIO
```

Solid arrows show the main API and packet-path relationships; the dotted arrow
shows rtnetlink configuration through `control` and the owning components.
`Service` coordinates IP processing and `Router`; `poller` advances RX, protocol
timers, and TX through kworkng or socket-assist polling. AF_PACKET receives frame
subscriptions from `EthernetDevice`, while Unix and vsock use their own transports.

| Component | Responsibility |
|---|---|
| posix/net · kernel clients | Call the socket API from POSIX syscalls or kernel code. |
| Socket API · SocketOps | Provides common operations across socket implementations. |
| TCP / raw IP | Implements TCP and raw IP sockets using smoltcp. |
| UDP | Manages UDP PCBs and queues, using knet's IPv4 path for packet I/O. |
| AF_PACKET | Receives Ethernet frame subscriptions and sends frames through Router. |
| Unix | Provides local stream and datagram communication. |
| netlink | Delivers kobject uevents and handles supported rtnetlink queries and configuration changes. |
| vsock | Provides optional guest-host stream communication through virtio-vsock. |
| smoltcp | Provides IPv4/IPv6 processing for the smoltcp socket path. |
| knet IPv4 / ICMPv4 | Validates and reassembles IPv4 packets, delivers UDP input, and handles ICMPv4 errors and responses. |
| DeviceRegistry | Owns registered interfaces and address configuration, validates mutations, and synchronizes device address projections and aggregate MTU. |
| Router | Selects routes, fragments outgoing IPv4 packets, and dispatches RX/TX queues across devices. |
| StackDevice | Borrows the admitted RX queue, data TX queue, and MTU for smoltcp; has no access to routes or physical devices. |
| LoopbackDevice | Returns locally transmitted packets to the receive path. |
| EthernetDevice | Adapts kclass network devices to knet's NetDevice interface and handles Ethernet and ARP. |
| kclass network device · NIC driver | Exposes the registered NIC and performs hardware packet I/O. |
| kclass vsock device · virtio-vsock driver | Exposes the registered vsock device and transports guest-host packets. |

`Router::stack_device()` constructs a temporary `StackDevice` for interface
initialization, ingress, and egress. Router still owns and serializes the queues;
this boundary separates the smoltcp adapter from routing and device dispatch
without changing lock order, queue capacity, or loopback delivery. A later queue
ownership change can replace the adapter's backing owner without exposing the
routing table to smoltcp again.

### IPv4 Header Views

`Ipv4Header` borrows header bytes, including options. `parse_header_only` checks
structure; `parse_input` also validates datagram bounds and the header checksum.
IPv4 processing trims padding and records the next-layer offset in `PacketBuf`;
`transport_packet()` reads from that offset to the packet end. Mutable network
access and truncation invalidate derived offsets, checksum state and UDP metadata.
Fragment data does not imply a complete transport header. ICMP quotes permit
truncated payloads; unfinished output derives its length from packet storage.

### IPv4 Filter Table Model

`netfilter/filter` stores IPv4 filtering policies as immutable tables of ordered
rules. Each table has INPUT, FORWARD, and OUTPUT chains for the LOCAL_IN,
FORWARD, and LOCAL_OUT hooks, plus named user chains. Rules hold match conditions
and a target; jumps refer to user chains by table-local `ChainId` values.
Match conditions live in `matcher.rs`; rules, chains, and tables live in
`rule.rs`, `chain.rs`, and `table.rs`, respectively. Their construction errors
are defined in `errors.rs`.

Rules validate extension counts and protocol compatibility, chains validate
their names, and tables enforce name uniqueness, hook coverage, aggregate limits,
and jump-graph constraints. Tables allow at most 256 chains, 4096 rules,
16 extensions per rule and 28-byte chain names.
Jump nesting is bounded by the chain count and the acyclic-graph requirement.
Chain-name lengths count UTF-8 bytes and follow the legacy iptables limit;
extension counts are an X-Kernel resource limit.

## Calling Constraints and Execution Context

`knet` runs in kernel context, but different code paths have different execution-context requirements. Callers must follow the constraints below.

* **Initialization order**

  `init_network` must complete before Internet sockets access `SERVICE`, `SOCKET_SET`, or `LISTEN_TABLE`, or routing and protocol processing begin. Unix sockets use their own transport and binding state; vsock initialization is handled separately by `init_vsock`. It initializes `SERVICE`, `SOCKET_SET`, `LISTEN_TABLE`, and the initial Router state. The dynamic workqueue used by `knet-poller` is started only after these objects are initialized.

* **vsock startup and callbacks**

  `init_vsock` installs the available raw transport and registers availability/removal callbacks. `start_vsock_bridge` must run after all scheduler run queues are initialized because it starts bridge tasks. Device availability and removal callbacks take sleepable locks. `publish_kobject_uevent`, `send_link_frame`, and relay operations also allocate or acquire sleepable locks and belong in task context. The periodic `Service::handle_timer_tick` callback uses atomic deadline checks, waiter notification, and IRQ-safe poller scheduling.

* **Normal protocol processing runs in task context**

  Paths such as `NetworkPoller::assist_once`, socket `send` / `recv` / `connect` / `accept`, and netlink mutations may acquire `Mutex` or `RwLock` objects and may execute relatively heavy data-plane work. These paths must run in normal task context.

* **IPv4 filter model construction**

  Construct filter policies in task context after allocator initialization.
  Table validation allocates temporary storage.

* **IRQ handlers only signal RX work**

  A device interrupt handler should only acknowledge the device interrupt, mark RX work as pending, and schedule the `NetRx` softirq. It must not run full protocol processing or blocking socket operations directly.

* **`NetRx` may deliver loopback UDP, but must not call `NetworkPoller::assist_once`**

  `NetRx` runs in a non-sleepable context.

  For Ethernet devices, it only consumes pending RX sources, wakes the corresponding device RX `PollSet`, and schedules `knet-poller`. The actual data-plane processing then runs in kworkng task context and is bounded by the configured number of poll rounds.

  Loopback UDP uses a more direct path. Before BH is disabled, the transmit path validates the UDP packet and stores the source/destination information and payload range in the existing `PacketBuf` control metadata. The same `NetRx` action then drains only these prepared handles from `pending_udp`, looks up the PCB through the `SpinNoIrq` registry, moves the packet into a receive queue with reserved capacity, and wakes the socket waiter. The hot UDP delivery path therefore performs no heap allocation.

  TCP, ICMP, IPv6, fragmented packets, and UDP packets that do not match a socket remain in `deferred`. They are later drained by the task poller through `poll_rx`.

  Code running in softirq context must not call `NetworkPoller::assist_once` or acquire sleepable locks owned by `Service` or `SocketSet`.

* **Blocking operations depend on poll and waker semantics**

  Blocking socket operations rely on `PollSet`, wakers, and timeout registration.

  `Pollable::register` may register sources only through the caller-provided `PollContext`. The corresponding `PollRegistrations` must remain alive while the operation is in the `Pending` state. The caller must also recheck readiness after registration. This preserves cancellation cleanup and closes the race between the readiness check and source registration.

  A blocking Unix stream `connect` waits for listener backlog capacity through `event-listener` when the backlog is full. The wait is bounded by `SO_SNDTIMEO` and therefore requires scheduler support and an interruptible task context.

* **Global state is not tied to a fixed current process or thread**

  Shared access to `SERVICE`, `SOCKET_SET`, and Router state is mainly protected by global locks and atomic state. Syscall-specific semantics are still provided by `posix/net`, which supplies the process file-descriptor context and caller credentials.

  Netlink sends and Unix datagram sends take a snapshot of the current caller credentials when the operation begins. Credentials are not cached in the socket itself.

* **Pathname Unix sockets require filesystem and credential context**

  Userspace `bind` and `connect` operations for pathname Unix sockets must run in a task that has a current thread and a valid filesystem context.

  Kernel callers without a current userspace task must use `bind_with_cred` to provide credentials explicitly, and the calling environment must still provide a valid filesystem context.

* **Concurrency is limited by global serialization points**

  Multiple execution paths may enter the crate concurrently. Critical access to `Service`, `SocketSet`, and listener backlog state is serialized by their corresponding locks.


## State Machines

### TCP Socket State

`TcpSocket` stores `socket::state::State` in `StateLock`. `Idle` permits initial setup, `Busy` serializes a transition, `Connecting` waits for the handshake, `Connected` represents an established endpoint, `Listening` owns a listener entry, and `Closed` records local full shutdown or connection failure. smoltcp maintains the separate TCP wire-protocol state; peer closure alone does not update this local enum.

```mermaid
%%{init: {
  "theme": "base",
  "themeVariables": {
    "fontFamily": "Inter, ui-sans-serif, system-ui, sans-serif",
    "fontSize": "15px",
    "background": "#FAF9F6",
    "primaryColor": "#F5F2EC",
    "primaryBorderColor": "#CFC8BD",
    "primaryTextColor": "#2E2B28",
    "lineColor": "#9A9187"
  }
}}%%

stateDiagram-v2
    direction LR

    [*] --> Idle

    Idle --> Busy : bind / connect / listen

    Busy --> Idle : bind succeeds
    Busy --> Connecting : connect starts
    Busy --> Listening : listen ok
    Busy --> PreviousState : operation error

    Connecting --> Connected : smoltcp established

    Connecting --> Closed : poll_connect observes failure

    Listening --> Listening : accept child
    Listening --> Busy : shutdown_inner
    Busy --> Closed : listener shutdown succeeds

    Connected --> Closed : shutdown Both / drop
    Closed --> [*]

    state "Previous state" as PreviousState

    classDef initialState fill:#F7E9E2,stroke:#D97757,color:#5C3428,stroke-width:1.5px;
    classDef activeState fill:#FBFAF8,stroke:#CFC8BD,color:#2E2B28,stroke-width:1px;
    classDef connectedState fill:#F3F0EA,stroke:#B8AA99,color:#2E2B28,stroke-width:1.2px;
    classDef closedState fill:#EEE9E1,stroke:#BDAF9F,color:#514A43,stroke-width:1.2px;
    classDef rollbackState fill:#FFF8F2,stroke:#D9B99B,color:#6A4A36,stroke-width:1px;

    class Idle initialState;
    class Busy,Connecting,Listening activeState;
    class Connected connectedState;
    class Closed closedState;
    class PreviousState rollbackState;
```

| From | To | Trigger |
|---|---|---|
| `Idle` | `Busy` | `bind`, `connect`, or `listen` acquires `StateLock::lock`. |
| `Busy` | `Idle` | `bind` succeeds and records the local endpoint. |
| `Busy` | `Connecting` | `connect` starts the smoltcp connection. |
| `Busy` | `Listening` | `listen` registers the socket in `ListenTable`. |
| `Connecting` | `Connected` | `poll_connect` observes smoltcp `Established`. |
| `Connecting` | `Closed` | `poll_connect` observes a state other than `SynSent` or `Established`. |
| `Connected` | `Closed` | `shutdown_inner` handles `Shutdown::Both`, including the file-drop path. A directional shutdown only changes its shutdown flag. |
| `Listening` | `Busy` | `shutdown_inner` acquires the listener state lock. |
| `Busy` | `Closed` | Listener shutdown removes its entry and bound endpoint. |
| `Busy` | Previous state | The closure passed to `StateGuard::transit` returns an error. |

Accepted children start in `Connected`; `accept` leaves the listening socket in `Listening`.

### UDP Socket State

`UdpSocketState::lifecycle` is an `RwLock<UdpSocketLifecycle>` initialized to `Init`. Its setters define these local transitions:

| Update | Resulting state |
|---|---|
| `set_local_endpoint(Some(...))` | `Bound`, with a local endpoint recorded. |
| `set_local_endpoint(None)` | `Init`, with the local endpoint cleared. |
| `set_peer_endpoint(Some(...))` | `Connected`, with peer filtering enabled. |
| `set_peer_endpoint(None)` | `Bound` if a local endpoint remains, otherwise `Init`. |
| `shutdown(Shutdown::Both)` | `Closed`; directional shutdown updates its atomic flag without changing the lifecycle. |

These setters assign the result from their arguments and current endpoint fields. The lifecycle records socket setup and readiness, independently of the PCB's registry membership.

### Unix Stream Listener Lifecycle

The following diagram uses descriptive lifecycle labels, not Rust enum variants. `ListenerState` stores `is_listening`, `is_receive_shutdown`, pending requests, and capacity reservations; the socket binding and optional channel distinguish unbound, bound, and connected endpoints. `listen` enables reception, read shutdown rejects new requests while retaining queued ones, and `close` clears listening state and drains the queue.

```mermaid
%%{init: {
  "theme": "base",
  "themeVariables": {
    "fontFamily": "Inter, ui-sans-serif, system-ui, sans-serif",
    "fontSize": "15px",
    "background": "#FAF9F6",
    "primaryColor": "#F5F2EC",
    "primaryBorderColor": "#CFC8BD",
    "primaryTextColor": "#2E2B28",
    "lineColor": "#9A9187"
  }
}}%%

stateDiagram-v2
    direction TB

    [*] --> Unbound
    Unbound --> Bound : bind
    Unbound --> Connected : connect

    Bound --> Connected : connect
    Bound --> Listening : listen

    Listening --> ReceiveShutdown : shutdown read

    note right of Listening
        enqueue / accept
        stays Listening
    end note

    Bound --> Closed : drop
    Connected --> Closed : drop
    Listening --> Closed : drop
    ReceiveShutdown --> Closed : drop

    Closed --> [*]

    classDef initialState fill:#F7E9E2,stroke:#D97757,color:#5C3428,stroke-width:1.4px;
    classDef normalState fill:#FBFAF8,stroke:#D8D1C7,color:#3A3632,stroke-width:1px;
    classDef activeState fill:#F5F2EC,stroke:#CFC8BD,color:#2E2B28,stroke-width:1.1px;
    classDef closedState fill:#EEE9E1,stroke:#BDAF9F,color:#514A43,stroke-width:1.2px;

    class Unbound initialState;
    class Bound normalState;
    class Listening,Connected,ReceiveShutdown activeState;
    class Closed closedState;
```

### vsock Connection State

With `vsock` enabled, `connection_manager::ConnectionState` records protocol state under each connection's mutex. `Idle` is a bound endpoint, `Listening` owns a listen queue, `Connecting` awaits the peer response, `Connected` permits data transfer, and `Closed` records shutdown or disconnection.

| Origin | Result | Local trigger |
|---|---|---|
| New bound endpoint | `Idle` | `VsockStreamTransport::bind` creates the manager entry. |
| Bound `Idle` endpoint | `Listening` | `listen` registers a listen queue and updates the connection. |
| New outbound connection | `Connecting` | Stream `connect` or `connection_manager::create_bridge_connection` creates an outbound entry. |
| Incoming connection request | `Connected` | The manager accepts a request on a listening or bridge port and creates a child entry. |
| `Connecting` | `Connected` | A `VsockTransportEventKind::Connected` event updates the connection and wakes waiters. |
| Existing connection | `Closed` | Peer disconnection, raw-transport detachment, or stream shutdown closes the entry and wakes affected waiters. |

`VsockStreamTransport` also uses the shared `StateLock` for API admission. Bind performs `Idle -> Busy -> Idle`, listen performs `Idle -> Busy -> Listening`, and connect performs `Idle -> Busy -> Connecting`; a failed guarded operation restores the previous state. Accepted transports start in `Connected`. Subsequent handshake completion and shutdown update the manager's `ConnectionState`; I/O and readiness read that state separately from the transport's admission lock.

With `vsock_tipc_bridge`, `BridgeConnectionState` tracks the TIPC side in `bridge_connection.rs`:

| Origin | Result | Local trigger |
|---|---|---|
| New host-originated entry | `VsockOnly` | `BridgeConnection::new` creates an entry before TIPC attachment. |
| New TIPC-originated entry | `TipcOnly` | `new_tipc_only` attaches the accepted TIPC channel while outbound vsock is pending. |
| `VsockOnly` | `TipcConnecting` | `connect_tipc` attaches the channel returned by `ipc_port_connect_async`. |
| `TipcConnecting` | `Active` | `on_tipc_ready` receives TIPC readiness. |
| `TipcOnly` | `Active` | `on_connected` receives outbound vsock completion. |
| `Active` or `TipcSendBlocked` | `TipcSendBlocked` | `tipc_try_send` receives `WouldBlock` while sending the staged record. |
| `Active` or `TipcSendBlocked` | `Active` | `tipc_try_send` finds no pending bytes or sends the complete staged record. |

`VsockOnly` waits for the dynamic service name on port 0; `TipcSendBlocked` retains one record for retry. `Invalid`, `TipcClosed`, and `Closed` are declared but unused variants. Cleanup removes the entry and closes its channel rather than transitioning through those variants.

### Kobject uevent delivery

`NETLINK_KOBJECT_UEVENT` sockets subscribe by binding a multicast group. Kernel
publishers append `SEQNUM` and enqueue each message only for matching groups.
The credential path crosses three knet modules. `GeneralOptions` in
`socket/general` stores the receiver's `SO_PASSCRED` selection.
`NetlinkSocket::recv`, the `SocketOps::recv` implementation invoked by POSIX
`recvmsg`, first verifies that the socket protocol is
`NETLINK_KOBJECT_UEVENT`, then reads that option for each dequeued kernel
uevent and constructs a trusted
`UnixCredentials { pid: 0, uid: 0, gid: 0 }` sender identity. Other netlink
protocols never receive these synthetic credentials. The receive path wraps
that value in the public `KernelAncillaryData::Credentials` variant from
`socket/mod` and appends it to the receive ancillary-data vector. The POSIX
layer consumes that variant and serializes it as `SCM_CREDENTIALS`, allowing
udev-compatible receivers to verify the kernel publication path without
consulting current-process credentials.

User-space uevent receivers commonly install a classic BPF filter. The POSIX
socket-option layer accepts attach/detach only on kobject-uevent sockets,
bounds the program at 4096 instructions, verifies the final return and all
jump targets, and treats the accepted filter as a delivery optimization. The
current knet queue does not execute the filter, so receivers may see additional
valid kernel uevents and must retain their ordinary subsystem checks.
`NetlinkSocket::is_kobject_uevent` is the public protocol query used by that
POSIX option layer to restrict this classic-BPF compatibility behavior to
`NETLINK_KOBJECT_UEVENT` sockets.

### Rtnetlink Control Plane

The rtnetlink control plane provides the interface for querying and modifying network configuration through netlink messages. It handles operations on links, IPv4 addresses, routes, and neighbor state, reads live snapshots from the corresponding network owners, and forwards configuration changes to `Service`, `Router`, or `NetDevice` as appropriate.

```mermaid
%%{init: {
  "theme": "base",
  "themeVariables": {
    "fontFamily": "Inter, ui-sans-serif, system-ui, sans-serif",
    "fontSize": "15px",
    "background": "#FAF9F6",
    "primaryColor": "#F5F2EC",
    "primaryBorderColor": "#CFC8BD",
    "primaryTextColor": "#2E2B28",
    "lineColor": "#9A9187",
    "edgeLabelBackground": "#FAF9F6"
  }
}}%%

flowchart LR

    CONFIG["init_network<br/>rtnetlink / ioctl"]
    SERVICE["Service"]
    ROUTER["Router"]

    LINK["Link state"]
    ADDR["IPv4 addresses"]
    ROUTE["IPv4 routes"]
    NEIGH["Neighbor state"]

    QUERY["RTM_GETLINK<br/>RTM_GETADDR<br/>RTM_GETROUTE"]

    CONFIG --> SERVICE
    SERVICE --> ROUTER

    ROUTER --> LINK
    ROUTER --> ADDR
    ROUTER --> ROUTE
    ROUTER --> NEIGH

    ADDR -->|"derive"| ROUTE

    LINK --> QUERY
    ADDR --> QUERY
    ROUTE --> QUERY

    classDef entry fill:#F7E9E2,stroke:#D97757,stroke-width:1.2px,color:#5C3428;
    classDef core fill:#EFEAE2,stroke:#B8AA99,stroke-width:1.3px,color:#2E2B28,font-weight:600;
    classDef state fill:#FBFAF8,stroke:#D8D1C7,stroke-width:1px,color:#3A3632;
    classDef query fill:#F2F4F1,stroke:#BFC6BC,stroke-width:1px,color:#364238;

    class CONFIG entry;
    class SERVICE,ROUTER core;
    class LINK,ADDR,ROUTE,NEIGH state;
    class QUERY query;

    linkStyle default stroke:#9A9187,stroke-width:1.15px;
```

| From | To | Trigger |
|---|---|---|
| Device link configuration | Updated device link configuration | `RTM_NEWLINK` updates the target `NetDevice` through `Service::update_device_link`. |
| Device link configuration | `RTM_NEWLINK` response | An `RTM_GETLINK` dump reads all `LinkSnapshot` values; a single-object query reads the matching snapshot by interface index or name. |
| Initial IPv4 address entry | `DeviceEntry::ipv4` | `init_network` registers the address through `Router::add_ipv4_addr`; Router also creates the corresponding local and connected routes. |
| IPv4 address entry | smoltcp, `IngressProcessor`, and device address projections | `DeviceRegistry` refreshes device projections; `Service` refreshes smoltcp and ingress views when an address is added, removed, or its owning device is removed. |
| Last owner of an IPv4 address | Router configured routes | When the address is removed, configured routes using that address as `prefsrc` are also removed. |
| Removed device | Router routes and device neighbor state | `unregister_netdev` holds `network_config_lock`; Router removes routes, addresses, and neighbors associated with the interface; other interface indices remain unchanged. |
| `RTM_GETADDR` | Router address snapshot | rtnetlink reads `Service::ipv4_addr_snapshots` directly and returns only the specified device's addresses when the request includes an interface index. |
| Initial route | `RouteTable` | `init_network` inserts the boot route directly into the table. |
| `RTM_NEWROUTE` / `RTM_DELROUTE` / `SIOCADDRT` / `SIOCDELRT` | `RouteTable` | Route updates are validated and applied by the table under the Router lock. |
| `RTM_NEWNEIGH` | Target `NetDevice` | rtnetlink forwards the update through `Service` and `Router` to the device neighbor table. |

### Network Data-Plane Poller

```mermaid
%%{init: {
  "theme": "base",
  "themeVariables": {
    "fontFamily": "Inter, ui-sans-serif, system-ui, sans-serif",
    "fontSize": "15px",
    "background": "#FAF9F6",
    "primaryColor": "#F5F2EC",
    "primaryBorderColor": "#CFC8BD",
    "primaryTextColor": "#2E2B28",
    "lineColor": "#9A9187"
  }
}}%%

stateDiagram-v2
    direction LR

    IDLE --> SCHEDULED : notify
    SCHEDULED --> RUNNING : acquire
    RUNNING --> RUNNING_PENDING : notify
    RUNNING --> IDLE : finish without work
    RUNNING --> SCHEDULED : finish with work
    RUNNING_PENDING --> SCHEDULED : finish

    classDef idleState fill:#F7E9E2,stroke:#D97757,color:#5C3428,stroke-width:1.4px;
    classDef normalState fill:#FBFAF8,stroke:#D8D1C7,color:#3A3632,stroke-width:1px;
    classDef activeState fill:#F5F2EC,stroke:#CFC8BD,color:#2E2B28,stroke-width:1.1px;
    classDef pendingState fill:#FFF8F2,stroke:#D9B99B,color:#6A4A36,stroke-width:1px;

    class IDLE idleState;
    class SCHEDULED normalState;
    class RUNNING activeState;
    class RUNNING_PENDING pendingState;
```

| Current State | Operation | Next State | Wake-up Behavior |
|---|---|---|---|
| `IDLE` | `notify` | `SCHEDULED` | Queues `knet-poller` work. |
| `SCHEDULED` | `notify` | `SCHEDULED` | The existing wake-up remains valid. |
| `SCHEDULED` | A kworkng callback or assist acquires execution ownership | `RUNNING` | The current executor runs one round or a bounded batch of rounds. |
| `RUNNING` | `notify` | `RUNNING_PENDING` | The current executor is responsible for scheduling follow-up work when it finishes. |
| `RUNNING_PENDING` | `notify` | `RUNNING_PENDING` | The pending state is preserved. |
| `RUNNING` | Finishes with no immediate work remaining | `IDLE` | No wake-up is issued. |
| `RUNNING` | Finishes with immediate work remaining | `SCHEDULED` | Queues `knet-poller` work for the next round. |
| `RUNNING_PENDING` | Finishes | `SCHEDULED` | Queues `knet-poller` work for the next round. |

`SCHEDULED` means that pending work has been published for further processing. `RUNNING` means that a single executor currently owns the right to run the poller, while `RUNNING_PENDING` records new notifications that arrive during execution. `PollState` encodes these conceptual states with `is_running` and `has_pending` under one `SpinNoIrq` lock. Completion releases execution ownership and combines budget exhaustion with notifications received while running. Follow-up submission occurs outside the lock.

In the `SCHEDULED` state, the reusable static Work of `knet-poller` and socket assist compete for execution ownership of the next batch. Loopback UDP does not depend on poller ownership; its transmit path completes xmit and `NetRx` delivery within the BH-disabled window.

`Interface::poll_at` and TCP deferred close share the protocol timer source, while the IPv4 reassembly queue uses a separate timer source. When a timer expires, it schedules the poller and wakes tasks waiting for socket readiness.

## Algorithms

### IPv4 Filter Jump-Graph Validation

The jump graph is checked with iterative topological traversal. Traversal
counts processed chains to detect cycles. It visits the entire table,
including user chains unreachable from built-in hooks. Acyclic paths are
accepted up to the table's chain-count limit.

### Prepared Polling Work

`init_network` initializes the static `NetworkPoller` before installing NIC
notification sources. Its embedded Work is reused for the kernel lifetime.
After network state initialization, `start` binds a private `NormalQueue` to
the boot CPU set with `CurrentCpu` routing and a per-CPU active limit of one.
The poller state lock additionally serializes actual network polling across
CPUs and foreground assistance. No state lock is held during queue submission
or network processing. Notifications arriving before queue readiness retain
pending demand and are replayed by `start`. These IRQ-side notifications do
not allocate submission storage or execute the data plane.

### Network Initialization

`init_network` always creates loopback with `127.0.0.1/8`. It also attaches one registered NIC when available, using the configured IPv4 address and default gateway. Router address entries generate local and connected routes; `Service::new` synchronizes the IPv4 views used by smoltcp and ingress.

**Algorithm 1. Network Initialization**

**Input:** Registered NIC handles and IPv4 configuration.

**Output:** Initialized network service, sockets, and poller.

| Line | Operation |
|---:|:---|
| 1 | **Procedure** `init_network()` |
| 2 | &emsp;`nics = kclass.net_devices()` |
| 3 | &emsp;`router = Router.new()` |
| 4 | &emsp;`lo = router.add_device(LoopbackDevice.new())` |
| 5 | &emsp;`router.add_ipv4_addr(lo, 127.0.0.1/8, host_scope)` |
| 6 | &emsp;**if** `nics.pop()` yields `nic`: |
| 7 | &emsp;&emsp;`eth = router.add_device(EthernetDevice.new("eth0", nic))` |
| 8 | &emsp;&emsp;`router.add_ipv4_addr(eth, configured_ipv4, universe_scope)` |
| 9 | &emsp;&emsp;`router.add_default_route(eth, configured_gateway)` |
| 10 | &emsp;&emsp;`subscribe_network_unregister(nic.id)` |
| 11 | &emsp;`SERVICE.init_once(Service.new(router))` |
| 12 | &emsp;`SOCKET_SET.init_once(SocketSetWrapper.new())` |
| 13 | &emsp;`LISTEN_TABLE.init_once(ListenTable.new())` |
| 14 | &emsp;`init_udp_registry()` |
| 15 | &emsp;`network_poller.start()` |
| 16 | &emsp;`SERVICE.start_timer()` after service and poller publication |

Source: [`init_network`](../src/lib.rs) and [`Service::new`](../src/runtime/service.rs). The pseudocode abbreviates address and route construction.

### RX Progress

Ethernet RX notifications use `NetRx` to wake RX poll sources and schedule `knet-poller`. Prepared loopback UDP packets have a separate softirq delivery path; deferred loopback packets enter task-context polling. Socket assist executes one already-scheduled round. Background work and assist acquire the same execution right through `SCHEDULED -> RUNNING`; notifications received during execution are retained as `RUNNING_PENDING`.

Each `Service::poll_budgeted` round first attempts pending IPv4 reassembly expiration and flushes retained ingress/control batches. It then dispatches queued TX, drains device RX, advances smoltcp, and uses the remaining TX and device-RX budgets for a second pass. Device RX and smoltcp ingress each have a separate allowance of `budget.rx_packets`.

Device draining rotates its starting position across devices, consuming each device until it is empty or the batch budget is exhausted. Its budget and progress counters include every successfully consumed Ethernet frame, including ARP and drops, so a stream without deliverable IP packets cannot bypass the batch bound or the service's time checks between batches. Only returned IP packets enter the ingress batch. Loopback retains its separately bounded stamped-UDP fallback before polling one deferred packet. IPv4 validation, destination filtering, reassembly, and UDP delivery run after releasing Router. Unmatched unicast UDP can generate ICMP port-unreachable responses; unsupported unicast IPv4 protocols can generate protocol-unreachable responses. ICMP errors are inspected for UDP before eligible packets enter the smoltcp batch. IPv6 enters that batch directly.

The following pseudocode summarizes the round. Failed shared-state `try_lock` acquisitions retain pending batches and return `has_more = true`. Contention during reassembly expiration only defers that timer work, allowing the round to continue. Reusable batch mutexes and nested listener operations still use blocking locks.

**Algorithm 2. RX Progress**

**Input:** RX budget $B_{\mathrm{rx}}$, TX budget $B_{\mathrm{tx}}$, timer budget $B_{\mathrm{timer}}$, and retained packet batches.

**Output:** Progress counters and the immediate-work flag `has_more`.

| Line | Operation |
|---:|:---|
| 1 | **Procedure** `poll_budgeted(budget)` |
| 2 | &emsp;start = monotonic_time() |
| 3 | &emsp;lock reusable RX, accepted, and control batches |
| 4 | &emsp;try to expire pending IPv4 fragments within $B_{\mathrm{timer}}$ |
| 5 | &emsp;flush retained batches, or return retry |
| 6 | &emsp;tx_done = dispatch queued TX within $B_{\mathrm{tx}}$ |
| 7 | &emsp;drain_device_rx($B_{\mathrm{rx}}$ unless time limit reached) |
| 8 | &emsp;`try_lock` socket set, then Router, then Interface; otherwise return retry |
| 9 | &emsp;`Interface.poll_maintenance()` |
| 10 | &emsp;**if** time limit has not been reached: |
| 11 | &emsp;&emsp;poll smoltcp ingress within $B_{\mathrm{rx}}$ |
| 12 | &emsp;call `Interface.poll_egress()` at least once |
| 13 | &emsp;**repeat** while progress continues, within $\max(B_{\mathrm{tx}}, 1)$ passes and the soft time limit |
| 14 | &emsp;reap deferred TCP closes and read `Interface.poll_at()` |
| 15 | &emsp;release Interface; refresh TCP acceptors; release socket set |
| 16 | &emsp;dispatch queued TX within remaining TX budget if time remains |
| 17 | &emsp;release Router |
| 18 | &emsp;**if** time and device-RX budget remain: |
| 19 | &emsp;&emsp;drain_device_rx(remaining device-RX budget) |
| 20 | &emsp;publish the earliest protocol / deferred-close deadline |
| 21 | &emsp;`try_lock` Router, or return retry |
| 22 | &emsp;**return** progress with `has_more` for pending RX, ingress, TX, immediately due protocol work, or pending reassembly expiration |
| 23 | **Procedure** `drain_device_rx(remaining)` |
| 24 | &emsp;process retained RX batch and flush accepted/control batches |
| 25 | &emsp;**while** budget and time remain: |
| 26 | &emsp;&emsp;`try_lock` Router, or return retry |
| 27 | &emsp;&emsp;pull a bounded batch, limited by smoltcp ingress capacity |
| 28 | &emsp;&emsp;release Router |
| 29 | &emsp;&emsp;`try_lock` IngressProcessor, or retain batch and return retry |
| 30 | &emsp;&emsp;validate/filter IPv4, reassemble fragments, deliver UDP, collect ICMP responses and packets accepted for smoltcp |
| 31 | &emsp;&emsp;release IngressProcessor |
| 32 | &emsp;&emsp;flush accepted/control batches, or return retry |
| 33 | &emsp;&emsp;**stop** when device draining reports no more work |
| 34 | **Procedure** `flush accepted/control batches` |
| 35 | &emsp;`try_lock` socket set if accepted batch is nonempty |
| 36 | &emsp;`try_lock` Router; on failure retain both batches and return retry |
| 37 | &emsp;prepare TCP listeners and enqueue accepted packets |
| 38 | &emsp;enqueue control packets; drop a control packet if its TX queue is full |
| 39 | &emsp;release acquired locks |

The 1 ms limit is a soft bound checked between work batches. Maintenance and one egress call still run after bulk RX/TX reaches this limit, provided the required locks are acquired. Packets waiting in retained batches and FIFO queues remain for later rounds. The tail RX pass queues accepted smoltcp packets for a later round.

A background callback runs at most four rounds, each with fresh budgets; assist runs at most one. Completion reschedules immediate work or notifications received during execution. The periodic timer callback checks protocol and reassembly deadlines, wakes waiters, and publishes timer work. Future deadlines alone do not set `has_more`.

Sources: [`poller`](../src/runtime/poller.rs), [`Service`](../src/runtime/service.rs), [`IngressProcessor`](../src/stack/ingress.rs), and [`Router`](../src/stack/router.rs).

### TX Routing

UDP builds IPv4 packets in knet; TCP and raw sockets submit packets through smoltcp. UDP sends routed to loopback bypass the poller TX queue. Other UDP sends enter the data queue after fragmentation and capacity checks. smoltcp output enters the data queue, including loopback traffic, while generated ICMPv4 errors use the control queue. AF_PACKET sends link frames through Router directly to the selected device.

**Algorithm 3. TX Routing**

**Input:** UDP payload, destination, source/interface binding, and TX budget $B_{\mathrm{tx}}$.

**Output:** Transmitted or queued packets, errors, and dispatch progress.

| Line | Operation |
|---:|:---|
| 1 | **Procedure** `send_udp(payload, destination, bound_source, bound_interface)` |
| 2 | &emsp;lock Router |
| 3 | &emsp;check broadcast permission and select an active output route |
| 4 | &emsp;source = bound_source if set, otherwise route-selected source |
| 5 | &emsp;**if** output device is not loopback: |
| 6 | &emsp;&emsp;check data-queue capacity for the expected packet count |
| 7 | &emsp;build UDP checksum and IPv4 header |
| 8 | &emsp;choose DF from `IP_MTU_DISCOVER` and route MTU |
| 9 | &emsp;fragment oversized IPv4 output; return `EMSGSIZE` if DF forbids it |
| 10 | &emsp;**if** output device is loopback: |
| 11 | &emsp;&emsp;dispatch fragments immediately |
| 12 | &emsp;**else**: |
| 13 | &emsp;&emsp;enqueue all fragments with the bound interface, or return `WouldBlock` |
| 14 | &emsp;release Router |
| 15 | &emsp;network_poller().notify(Tx); network_poller().assist_once() |
| 16 | **Procedure** `dispatch_budgeted(tx_budget)` |
| 17 | &emsp;**repeat** up to $B_{\mathrm{tx}}$ times: |
| 18 | &emsp;&emsp;packet = pop control queue first, otherwise data queue |
| 19 | &emsp;&emsp;**stop** if both queues are empty |
| 20 | &emsp;&emsp;**if** IPv4: |
| 21 | &emsp;&emsp;&emsp;prepare IPv4 length/checksum fields |
| 22 | &emsp;&emsp;&emsp;**if** limited broadcast: |
| 23 | &emsp;&emsp;&emsp;&emsp;validate source and replicate to interfaces allowed by binding |
| 24 | &emsp;&emsp;&emsp;**else**: |
| 25 | &emsp;&emsp;&emsp;&emsp;look up longest-prefix route |
| 26 | &emsp;&emsp;&emsp;&emsp;validate bound interface and local source |
| 27 | &emsp;&emsp;&emsp;&emsp;send through route device to gateway or destination |
| 28 | &emsp;&emsp;**else if** IPv6 multicast: |
| 29 | &emsp;&emsp;&emsp;replicate to all devices |
| 30 | &emsp;&emsp;**else if** IPv6 unicast: |
| 31 | &emsp;&emsp;&emsp;look up route, verify source, and send through route device |
| 32 | &emsp;**return** processed count and whether TX or resulting RX work remains |

IPv4 limited broadcast is replicated subject to interface binding. Other IPv4 destinations, including multicast and directed broadcast, use route lookup. IPv6 multicast is replicated to all devices in this dispatch path. These rules differ from a general multicast fan-out policy.

`EthernetDevice` currently accepts IPv4 output only. It checks link state, source, and MTU, then sends limited and directed broadcasts to the Ethernet broadcast address. Other next hops use the ARP cache: a valid entry permits immediate transmission; an unresolved or expired entry triggers an ARP request when needed and retains the packet in bounded `pending_tx`. A full pending queue drops the packet. IPv6 remains available in the smoltcp and loopback paths, while the Ethernet adapter drops IPv6 output.

Sources: [`UDP send`](../src/transport/udp/socket.rs), [`Service::prepare_and_send_ipv4_packet`](../src/runtime/service.rs), [`Router`](../src/stack/router.rs), and [`EthernetDevice::send_ip_packet`](../src/device/ethernet.rs).

### Unix Stream Control Data Delivery

Unix stream control data (SCM_RIGHTS) is attached to **byte ranges**, not to
datagram boundaries. A send that carries `SendOptions.ancillary` registers a
record marked with the byte range `[start, end)` it is about to publish in the
direction's shared `DirectionAncillary`. The consumer delivers the record when
the `recv` that reaches that segment completes, and the read cap stops at the
segment's end so a single `recvmsg` consumes at most one record -- the same
stop point as Linux, which ends its read after the first fd-bearing skb.

**Algorithm 4. Byte-marked control delivery**

| Line | Operation |
|---:|:---|
| 1 | **send** registers the record `(start = sent_total, end = start + count, entries)` in `DirectionAncillary` before publishing the bytes. |
| 2 | **send** publishes the bytes through `advance_write_index`; the cursor `sent_total` advances with the registration. |
| 3 | A send registers its record only when it is about to publish at least one byte, immediately before `advance_write_index`. A failed read, a rejected liveness check, an exhausted record quota (`ENOBUFS`), or a closed consumer (`EPIPE`) publishes no byte and leaves no record. |
| 4 | **recv** caps its copy at `min(occupied, record.end - recv_total)` for the oldest pending record: the read may span plain data and stop at the record's committed segment end. |
| 5 | **recv** copies up to that cap, then retires the record its window reached (`pop_reached`) and delivers its entries to the caller. |
| 6 | On `MSG_PEEK`, the read stops at the same cap but the record is **duplicated** (the shared entry-handle `Arc`) and kept for the later consuming recv; the ring and cursor are not advanced. |
| 7 | On a consuming recv with a control buffer, the record is retired into `RecvOptions.ancillary`. |
| 8 | On a consuming recv **without** a control buffer, the reached record is dropped instead of being inherited by a later `recvmsg`, and `RecvOptions.ancillary_discarded` reports the loss so the POSIX layer sets `MSG_CTRUNC`. |

The registration-before-publish ordering at line 1 guarantees that a
concurrently reading consumer which observes the published bytes always sees
the registered segment and therefore never over-reads past it. Each
`AncillaryData` is an `Arc`, so `MSG_PEEK` duplicates the record by cloning
one handle (`scm_fp_dup`) without reopening the files, and the later
consuming `recv` redeploys the same open file descriptions.

The pending queue is a `VecDeque` retired from the front, so registering,
reaching, and draining a record are all O(1). Delivery and the endpoint
close move the entries out from under the `SpinNoPreempt` guard and deliver
or drop them only after it is released: extending the receiver's sink
allocates, and dropping the last file reference closes the file, which may
take a sleepable mutex such as a pipe's internal lock.

Retention in one direction is bounded: at most `MAX_PENDING_RECORDS` records
holding at most `MAX_PENDING_ANCILLARY` entries in total, with
`posix-net`'s per-`sendmsg` `SCM_MAX_FD` cap bounding the descriptors per
record. Registration fails with `ENOBUFS` once either bound is reached, so a
local peer cannot pin an unbounded number of file references behind a slow
consumer. When the consumer's endpoint closes, `Channel::drop` drains its
receive direction under the producer's `tx_order` lock, so the peer's passed
file references are released even if the records were never received; a pipe
write end passed but never received then lets the pipe reader observe EOF
instead of blocking forever. The drained queue also rejects later
registrations, which the send path reports as `EPIPE`.

Sources: [`unix/stream.rs`](../src/unix/stream.rs) and
[`unix/stream/channel.rs`](../src/unix/stream/channel.rs).

## Concurrency Model

Concurrency is organized around a single owner for protocol progress, with finer-grained locks protecting socket, queue, and control-plane state.

### Protocol Progress

The global `SERVICE` is a `LazyInit<Service>`. Separate mutexes protect the smoltcp `Interface`, the `Router`, the `IngressProcessor`, and each reusable RX, accepted-packet, and control-packet batch. The poller acquires the first three with `try_lock`. If any of these locks is contended, the poller retains the batch and reports that work remains immediately available. These batches are accessed only by the executor that owns the global protocol-progress right.

`NetworkPoller` represents this execution right with two locked booleans (`is_running`, `has_pending`) corresponding to four conceptual states: `IDLE`, `SCHEDULED`, `RUNNING`, and `RUNNING_PENDING`. A prepared kworkng background batch may process 512 RX packets, 256 TX packets, and 32 timer events. A socket-assisted batch uses smaller budgets of 16, 16, and 8. Every round also has a 1-ms soft time limit. Background batches run at most four consecutive rounds, while one assistance call runs at most one round. When a background batch reaches its round limit, it queues another batch and releases the execution right.

Protocol and IPv4 reassembly deadlines use separate `AtomicU64` fields. The periodic sampling callback is deliberately small: it checks the atomic deadline, wakes waiters, and notifies the poller. Each successful staged smoltcp poll refreshes the protocol timer through `Interface::poll_at`, so TCP retransmission and similar deadlines continue to advance even when socket waiters are blocked.

`Service` owns a `ktask::Timer<NetworkTimer>`, constructed once and started
after the global `SERVICE` and poller are published. The IRQ handler accesses
that permanent service, then explicitly rearms for completion time plus 10 ms
on the same CPU. Late samples do not trigger phase-aligned catch-up. Timer
registration and renewal reuse owned storage without allocating; this is not
an allocation-free claim for all network notification or protocol processing.
An out-of-range rearm logs an error and stops sampling instead of panicking
inside the IRQ. The core does not own a network interval or business counter.

The global `SOCKET_SET` wraps `SocketSetState` in a single mutex. The smoltcp socket set and TCP deferred-close metadata share this ownership lock, so socket registration, protocol progress, close, and reclamation never transfer a handle between two mutexes. TCP transition admission uses atomic compare-and-swap through `StateLock::lock`, returning the observed state if admission fails; `StateGuard::transit` publishes the result or restores the previous state with an atomic store. Socket options and shutdown flags use atomics only for configuration and shutdown state shared across threads.

### Receive Path and UDP Queues

The Router lock covers RX device pulls and TX dispatch. IPv4 validation, filtering, and UDP demultiplexing run after both the Router lock and the smoltcp socket-set lock have been released. TCP snooping runs once the accepted batch holds both locks, keeping listener side effects within the same batch handoff. The smoltcp ingress queue has fixed capacity, and each call to `poll_ingress_single` consumes one packet. Packets left behind because a budget or time limit was reached, or because a shared lock was contended, remain available for a later round.

`UDP_PCB_REGISTRY` contains 256 port-based buckets. Each bucket uses `SpinNoIrq` because `NetRx` performs PCB lookups in softirq context. The receive queue and connected peer of each PCB use the same lock type. Sleepable bind and connect state updates remain outside the bucket lock.

Before a packet enters a PCB queue, the loopback send path validates the datagram before entering BH context, or the ordinary ingress path validates it in task context. Address and payload-range parsing occurs at the same stage, and the results are stored in the packet's control metadata. The receive queue then stores only a pointer-sized `PreparedUdpPacket`, which retains the `PacketBuf` handle created when the packet entered the stack.

Each PCB reserves 1,024 `VecDeque` slots at creation. This follows the Linux `__udp_enqueue_schedule_skb` pattern, where `__skb_queue_tail` inserts an existing `skb` while holding `sk_receive_queue.lock`. Accordingly, `enqueue` performs only an occupancy check and inserts an existing handle while running in softirq context under `SpinNoIrq`. If the queue is full, it releases the queue lock before reclaiming the packet. `MSG_PEEK` increments the `PacketBuf` reference count under the lock and copies the payload after releasing it.

The shared `NET_RX_QUEUE` is also protected by `SpinNoIrq` and is used by loopback transmit, `NetRx`, and task-context `poll_rx`. Its `pending_udp` and `deferred` queues both contain pointer-sized `PacketBuf` values. `NetRx` removes already-annotated entries from `pending_udp` under its budget for UDP delivery; the task poller removes other packets from `deferred`. Capacity accounting includes both queue lengths and all in-flight packets, so producers cannot consume slots that are temporarily vacant while a packet is being delivered to a PCB. If a UDP packet matches no PCB, this path retains unique ownership, clears the packet metadata, and moves it to `deferred`.

`IngressProcessor::ipv4_reassembler` owns reassembly queues and is protected by `Service::ingress`, a `Mutex<IngressProcessor>`. The global `SERVICE` is a `LazyInit<Service>` with separate component locks.

The UDP socket lifecycle and local endpoint use separate `RwLock`s, while the peer endpoint uses `SpinNoIrq` for softirq lookup. The asynchronous-error queue has its own mutex; error status, shutdown flags, and receive-error enablement use atomics. Socket-local waiter sets report data, errors, write readiness, and shutdown independently of device RX notifications.

### TCP and Unix Stream Synchronization

`LISTEN_TABLE` stores listeners in a `Mutex<HashMap<...>>`, and each listener entry uses its own mutex for the backlog queue. The entry's `accept_poll` sits outside that mutex. `register_accept_waker` first registers with `accept_poll` without taking the entry lock, then briefly locks the entry to recheck readiness. This preserves the register-and-recheck sequence without running `Waker::clone` or other `PollSet` operations under the queue lock.

The poller revisits an entry after a new packet arrives or whenever its SYN queue is nonempty. A child that becomes acceptable during a later ingress or timer poll can then move from the SYN queue to the accept queue. `accept_poll` wakes its waiters only after the entry lock has been released.

TCP's `bound_endpoint` mutex protects one socket's local binding. `TCP_BOUND_ENDPOINTS` uses a mutex to serialize port-conflict accounting, and the ephemeral-port cursor is separately mutex-protected. Raw socket local and peer addresses and TTL use `RwLock`s; their smoltcp buffers remain under `SOCKET_SET`.

`ABSTRACT_BINDINGS` and `PATH_BINDINGS` use separate mutexes for Unix address lookup. Each `BindEntry` contains independently locked stream and datagram binding slots. `UnixDomainSocket` protects its local and peer addresses with mutexes. Datagram transports protect the receiver and binding slot with mutexes and the connected peer and local address with `RwLock`s; `async_channel` and poll sets handle queued delivery and notifications.

Unix stream synchronization is divided by responsibility:

- Each endpoint stores its channel in `channel: Mutex<Option<Channel>>`. This mutex serializes the final commit of `connect`, along with `send`, `recv`, `shutdown`, and channel release on the same endpoint.
- Each listener keeps its FIFO pending queue, reserved-capacity count, backlog limit, listening state, and read-shutdown state in `ListenerState` under a separate mutex. `request_available` wakes `accept`, while `capacity_available` wakes blocked `connect` calls. The binding-slot lock is held only long enough to clone the listener reference and is released before any capacity wait. The fixed lock order is `channel`, listener handle, and `ListenerState`.
- Each endpoint uses `tx_order: SpinNoPreempt<()>` as the ordering point for one transmit direction. Local write shutdown, peer read shutdown, write-index publication, and peer-side EOF detection on an empty queue all pass through this lock. Code acquires the `channel` mutex before a single `tx_order`, and never holds the ordering locks for both directions at once.

`send` first writes into an unpublished vacant region, then takes `tx_order` to recheck shutdown state and publish the new write index. If `recv` observes an empty queue, it rechecks the occupied length and shutdown state under the same ordering lock. `shutdown` publishes the completed state and clones the peer endpoint reference while holding the `channel` mutex, then releases the mutex before waking waiters. Listener-event notifications likewise occur outside both `ListenerState` and the source socket's `channel` lock, while user-data copies and `PollSet` wakeups occur outside `tx_order`.

Readable, writable, and connection-state events use separate `PollSet` instances for each endpoint. A write wakes the peer's readable waiters. A read wakes the peer's writable waiters when buffer occupancy crosses the send-buffer low-water mark. A half-close wakes the affected direction, and a full close notifies both endpoints through their connection-state waiters.

### Packet and vsock Synchronization

`PACKET_HANDLERS` protects weak socket registrations with a mutex and tracks the active count atomically. Each packet socket uses an `RwLock` for its binding, a mutex for its bounded RX queue, and a separate `recv_lock` to serialize receive/peek operations across payload copying. Packet and drop statistics use atomic counters. Frame publication snapshots active sockets before accessing their queues.

`VSOCK_DEV` protects the installed class-device handle, and `VSOCK_CONN_MANAGER` protects transport ownership, connection/listener maps, bridge ports, and queued events. Each `Connection` and `ListenQueue` has a separate mutex. Stream transports protect their connection references with mutexes and serialize sends with `tx_lock`; the manager never acquires that send lock. Paths that need nested device, manager, and connection access follow `VSOCK_DEV -> VSOCK_CONN_MANAGER -> Connection`. Sends snapshot transport and credit state and release manager/connection guards before driver I/O or credit waits.

`POLLER_STATE` protects the vsock polling reference count and running flag; the idle backoff uses an atomic counter. The bridge has separate mutexes for connections, deferred RX events, and published ports. Atomic flags coordinate startup, and an atomic token counter identifies TIPC handle-set entries. TIPC handle sets, poll sets, and wait queues notify bridge workers and socket waiters.

### Control Plane and RX Wakeups

The Router coordinates network configuration and dynamic source selection. `network/router.rs::RouteTable` owns configured and address-derived rules, validates configured-route updates against borrowed devices and addresses, and synchronizes address-derived routes by identity. `device/registry.rs::DeviceRegistry` owns device registration and exact address mutations, including capacity checks and device projection synchronization. Router retains legacy first-address, broadcast, and netmask update policies and applies them through those operations. The outer `stack/router.rs::Router` reconciles route and pending-packet state from those mutation results under its existing mutex. Each device owns its neighbor table. Netlink handles protocol parsing and live-snapshot encoding, then calls the appropriate owner.

`control::network_config_lock` serializes control-plane mutations across sockets, legacy socket-ioctl mutations, and `unregister_netdev`. Each netlink socket has mutexes for its send transaction and RX queue and an `RwLock` for its local address. The global `UEVENT_SUBSCRIBERS` mutex protects weak subscriber references, and `UEVENT_SEQNUM` atomically allocates event sequence numbers. The send-transaction lock covers capacity preflight, mutation execution, and response enqueueing for one socket. Responses are generated outside the RX-queue lock; that lock is taken only to check capacity and enqueue the result. The fixed acquisition order is the send-transaction lock, `network_config_lock`, Router, ingress, Interface, and netlink RX queue, omitting any lock that the operation does not use.

`GeneralOptions` stores device binding in `bound_dev_if: AtomicI32`. UDP lookup and transmission and TCP/raw RX-device selection read this field with Relaxed ordering. Binding and unbinding use an atomic exchange; an actual change notifies `device_binding_changed: PollEvent`, whose Release/Acquire version ordering publishes the preceding field update. Repeated writes of the same interface index remain silent.

RX registration reads the event version, reads the interface index, registers device sources and the binding-change event, then rechecks the version. Changes during registration trigger a readiness recheck; changes while waiting trigger registration renewal. Each new `PollRegistrations` replaces the old device registrations, and cancellation or completion removes the binding-change subscription.

TCP and raw IP use `GeneralOptions::rx_device_selection` to select RX devices. Explicit `SO_BINDTODEVICE` selects the specified device; default and unbound states select all devices. Address binding, connection state, and route changes are independent of this selection, while the protocol receive path filters packet delivery.

`Service::register_rx_waker` uses the caller's `PollContext` to register the aggregate `timeout_poll` and any Ethernet RX poll source that supports interrupt-driven RX. When a device becomes pending, the `NetRx` softirq wakes the corresponding poll source and schedules `knet-poller` work for background protocol progress. TCP and raw IP waiters can also wake through that source. `PollRegistrations` keeps these registrations alive across the `Pending` state.

Loopback devices and Ethernet devices without an attached `NetRxScheduler` continue to use the aggregate `timeout_poll` waker, which supports broadcast wakeups to multiple tasks. The device layer does not retain raw caller wakers or register a separate, inequivalent task waker outside `Service`.

UDP receive-only waits subscribe to socket events in `UdpSocketWaiters`. Receive enqueueing, asynchronous errors, and read shutdown wake the corresponding waiters. UDP lookup applies address and device-binding filters, so existing receive subscriptions remain valid across binding changes. Ethernet RX softirq independently schedules `knet-poller`; prepared loopback UDP is enqueued by `NetRx` or the send path's task-context fallback. Sender TX notifications advance other loopback work, and protocol and reassembly deadlines independently notify the background poller. Receive-only waits therefore remain independent of device RX broadcasts and aggregate `timeout_poll` notifications. Send waits continue to subscribe to network-progress events, and combined read/write waits retain the device and timer subscriptions needed for TX. `PollRegistrations` owns all socket-event registrations, followed by a readiness recheck.

Ethernet RX source registration is protected by `NET_RX_SOURCES: SpinNoIrq<NetRxSources>`. Each source publishes pending work through an atomic flag, and the IRQ-safe scheduler keeps only a weak source reference. Device teardown detaches the scheduler and unregisters the source. Source identifiers and softirq availability also use atomics.

## Design Decisions

### UDP and IPv4 Data Paths

Crate-local types own UDP PCBs, bind-conflict checks, send and receive queues, IPv4 validation, fragment reassembly, output fragmentation, and ICMPv4 errors. TCP, raw IP, and IPv6 use the smoltcp socket set, driven by `knet-poller` and explicit `NetworkPoller::assist_once` calls. This boundary keeps UDP independent of smoltcp socket handles while preserving TCP and raw-socket semantics.

### Budgeted Processing

The `NetRx` softirq, socket TX paths, protocol timers, and socket assist share one `NetworkPoller`. Its IRQ-safe state lock protects pending demand and grants execution to a single owner through `PollState::claim`. Notifications received during execution set `RUNNING_PENDING`, causing completion to schedule another batch. Assist only consumes scheduled work; it neither creates events nor waits for ownership.

Loopback UDP is delivered directly to the PCB within the transmit path's bottom-half window, following the Linux sequence from `dev_queue_xmit` through `loopback_xmit` and `__netif_rx` to softirq processing at `local_bh_enable`. TCP, ICMP, and IPv6 use the poller's TX-then-RX rounds.

RX, TX, and timers have separate budgets, and each `Service` round has a 1 ms soft time limit. Staged smoltcp APIs bound ingress and egress work. TX stages share one budget, as do the main and final RX stages, allowing loopback TCP and ICMP to complete a device round trip within one assist. Control traffic takes priority over data traffic. Router, Interface, or socket-set contention ends the round with `has_more` set. IPv4 reassembly processing acquires the `IngressProcessor` lock only for pending expiry events; contention preserves those events while TX continues. Expiry events are processed under the timer budget across successive rounds.

TCP sends fill the smoltcp buffer and notify the poller, which advances the protocol and dispatches TX in bounded batches. TCP receives publish `RxWindow` when free buffer space before the read is below the maximum window-scaling quantum; subsequent window growth uses RX and timer processing. TCP and raw sockets register aggregate send wakers with smoltcp. `DataTxQueue` owns bounded data-TX packets and capacity waiters under the Router mutex. `Service` registers send waiters and wakes them after TX releases queue capacity and the Router lock; `NetworkPoller` owns execution scheduling. Separate atomic deadline sources track protocol and deferred-close timers and IPv4 reassembly. Periodic sampling wakes socket waiters and notifies the poller when deadlines expire.

### Socket File I/O

`SocketFileOps` adapts VFS source and destination iterators directly to the
socket's byte-buffer interfaces. Each file write invokes one socket send,
including empty datagrams, without splitting a message into generic scalar
I/O chunks. Each nonempty file read invokes one socket receive. An empty file
read returns zero without consuming a message. File nonblocking flags become
per-call send and receive flags, including on Unix datagram receives. POSIX
receive returns zero address length for an unbound Unix sender, while
getsockname retains the family-only representation of an unbound socket.

### Control-Plane State Ownership

The network device registry assigns stable `NetDeviceId` values independently of its storage slots. Device removal preserves surviving interface indices. The current allocator reserves index 1 for loopback and assigns monotonically increasing indices from 2 through `i32::MAX`, returning `ENOSPC` after exhaustion. Removing the loopback entry clears its registration flag so a subsequent loopback registration can use index 1 again.

Devices own link configuration and expose interface names, MTUs, administrative and operational states, and hardware addresses through `NetDevice::link_snapshot`. `DeviceRegistry` resolves link queries and validates link updates, including name uniqueness and per-device MTU limits. It refreshes the aggregate MTU on device registration, removal, and successful MTU changes. `Service` accesses the registry under the Router lock. The AF_PACKET send path obtains an allocation-free `LinkSendSnapshot` under the Router lock, validates it after releasing that lock, and reacquires the Router lock through `Service::send_link_frame` for transmission. Snapshot validation and dispatch are separate critical sections. Device-existence checks use `has_device`, and route MTU queries read the matching device directly. Configuration changes or device removal rebuild the smoltcp `Interface` only when the effective MTU changes, keeping cached `DeviceCapabilities` consistent with the device set while otherwise preserving runtime caches.

`DeviceEntry::ipv4` owns the interface address configuration defined in `network/interface_address.rs` and releases it on device removal. Each `Ipv4AddrEntry` has `addr`, `scope`, and optional `broadcast` fields; device identity is supplied separately. `Ipv4AddressSet` validates unicast addresses, maintains device-local CIDR uniqueness, and replaces or removes entries by CIDR while preserving device-local order. Rejected mutations leave entries unchanged. `DeviceRegistry` validates device existence and the address capacity supplied by the shared smoltcp stack; existing identities remain idempotent at capacity. Legacy address and netmask ioctls update the primary (first configured) entry while preserving other addresses on the device. Broadcast addresses default to the CIDR-derived value. Netmask updates retain scope and recompute the broadcast address only when its previous value was mask-derived. If the resulting CIDR already exists on the same device, the old entry is removed and the existing target retains its attributes; the operation succeeds, matching Linux `SIOCSIFNETMASK`.

Addresses are enumerated by device registration order, then by configuration order within each device. Device-local order determines primary-address and source selection; `RouteTable` independently preserves route precedence across address mutations.

`RouteTable` owns route identity and precedence. Each `RouteEntry` carries an `insertion_order`; entries are sorted by longest prefix, local host-route priority, and then oldest sequence. Their physical order is derived from these explicit fields, and entry indices have no stable identity. Lookup and control-plane snapshots traverse the sorted entries directly. Sorting runs on configuration paths; packet lookup allocates no memory.

`AddressRouteId` identifies address-derived routes by owner device, local/connected kind, and destination prefix. Derived routes use the main table; the output device is the owner for connected routes and loopback for local routes. Synchronization updates attributes of surviving identities while preserving their `insertion_order`, removes vanished identities, and gives newly created routes a new sequence. Multiple addresses on a device can support one local or connected route; the first remaining supporting address in per-device order supplies its preferred source. Local routes use a `/32` identity independent of address netmasks; connected routes use the network prefix and are omitted for `/32` and unspecified networks.

Address removal also clears configured routes whose preferred source becomes invalid and the device's pending TX. `Service` projects Router addresses into the smoltcp `Interface`, `IngressProcessor`, and devices; Ethernet uses them for ARP, directed broadcasts, and packets awaiting neighbor resolution.

Configured routes belong to `RouteTable`, whose update methods validate before modifying rules. Rejected replacements preserve the original rule and its sequence. Successful configured-route replacements retain the delete/add contract and receive a new sequence. For `RTM_NEWROUTE`, exclusive creation checks all matching routes, while replacement selects a matching configured route even when a derived route precedes it. Derived routes cannot be replaced through this interface. `RTM_NEWROUTE` and `SIOCADDRT` require gateways to lie within the output interface's IPv4 subnet and exclude directed-broadcast addresses. Ethernet devices own their neighbor tables, so route and neighbor mutations operate through their respective owners. `unregister_netdev` holds `network_config_lock` while Router removes the device's routes, addresses, and neighbors. Surviving interface indices remain unchanged.

### Credentials for Unix Pathname Sockets

Linux implicitly reads credentials from the task context through `current_cred()`. Since knet serves both user tasks and kernel callers during boot, it passes credentials explicitly. User-facing entry points take one credential snapshot at the start of an operation and pass `&Cred` through subsequent VFS path lookups. Kernel callers specify credentials through `bind_with_cred`.

Credentials follow the call path, keeping their lifetime separate from Unix sockets, path-lookup state, and dentries. `sock_alloc_file` explicitly receives `Arc<Cred>` and stores it in `VfsFile::cred`; the socket object retains only its own state. Entry points obtain the current task's credentials, and knet uses the supplied credentials throughout the operation.

### Credentials for rtnetlink Sends

Netlink sockets can be passed between processes, so permission checks use the credentials of each sender. POSIX send and socket-file write identify the protocol at entry. The netlink branch takes one current-credential snapshot and passes it through `Socket::send_with_cred` to rtnetlink permission checks. Kernel callers outside a user-task context specify credentials explicitly through `send_with_cred`.

### Credentials for Unix Datagram Messages

Unix datagram sockets, including the transport currently used for Unix `SOCK_SEQPACKET` socket pairs, can also be inherited or passed between processes. POSIX send and socket-file write use `UnixDomainSocket::is_datagram` to limit capture to datagram transport. Both call `unix::current_sender_credentials` once per send to capture the sending process's PID and real UID/GID into `SendOptions::unix_sender_credentials`. The helper requires a current user thread; stream sends do not call it. Kernel callers without a current user process may leave the snapshot unset.

`DgramTransport` stores the snapshot with the queued message instead of using the PID recorded when the socket was created. On receive, `GeneralOptions::pass_credentials` controls whether the transport appends `KernelAncillaryData::Credentials` to the returned ancillary data. The POSIX receive path serializes that value as `SCM_CREDENTIALS` and reports `MSG_CTRUNC` when the control buffer is absent or too small. Explicit user-supplied `SCM_CREDENTIALS` sends are not supported and return `EINVAL`; supporting them would require identity and privilege validation. This keeps process-context access at the user-facing boundary while ensuring that a child using an inherited socket is identified as the sender.

Unix datagram receive delivers kernel credentials before passed file descriptors so a short control buffer prioritizes sender identity. POSIX control-message output advances by the ABI-aligned message size while keeping cmsg_len unpadded; a final message may omit trailing padding when capacity is exhausted. If only part of a credential message fits, POSIX writes the header and the available native-endian PID/UID/GID bytes (including partial fields), reports the actual message length, and sets `MSG_CTRUNC`. A buffer smaller than the control header produces no control message. Missing trailing padding alone does not mean the credentials were truncated.

### Device Masks for RX Wakeups

TCP and raw IP derive their RX device selection from `SO_BINDTODEVICE`. Explicit binding narrows device subscriptions; default and unbound states select all devices. Waiters register with those devices and the smoltcp poll timeout, then use actual readiness to determine user-visible events. Address binding, connection state, and route changes leave device selection independent. UDP receive-only waits use socket-local events after demultiplexing applies the device-binding filter.

smoltcp's `poll_at` returns a deadline with the same epoch as the supplied timestamp, allowing the compatibility layer to map `SmoltcpInstant` directly to `MonotonicInstant`. This mapping preserves expired deadlines without signed-delay calculations or `as u64` conversions.

### TCP Listener and Connection Management

knet manages listening sockets separately from accepted child connections. `ListenTable` creates a child smoltcp socket when a SYN arrives and places the pending connection in the backlog queue, keeping POSIX accept semantics within knet.

The listener table therefore snoops initial TCP packets, cleans up aborted children, and rechecks connections in the SYN queue whenever the network makes progress. Network events and protocol deadlines drive subsequent polling. Pending connections in the SYN queue do not themselves request continuous polling, avoiding busy loops before a protocol deadline expires.

### Unix Stream Waiters and Send Ordering

A bidirectional Unix stream channel stores a separate `StreamEndpoint` for each end. Each endpoint uses atomics to track local read/write shutdown state and pending connection errors, while `StreamPollSets` maintains separate readable, writable, and connection-state waiter sets. `ListenerQueue` holds listener state and pending connections, notifying the endpoint's readable waiters when a connection request arrives.

Read events register with the readable set, write events with the writable set, and waiters interested only in connection state with the connection-state set. Each waiter uses the corresponding fixed-capacity set, avoiding duplicate registrations for the same direction. Writing data to the ring buffer notifies peer readers. Reading data notifies peer writers when send-buffer occupancy falls from above one-quarter capacity to below that threshold. When both connection directions close or an endpoint is released, all three sets are notified so that waiters can recheck readiness.

Each `StreamEndpoint` uses `tx_order` to coordinate data publication and shutdown in its send direction. Send commits the write index under this lock, shutdown publishes closure under the corresponding direction lock, and peer recv uses the same lock for its final empty-queue and EOF checks. This ordering ensures that the receiver observes published data before EOF. If shutdown acquires the direction lock first, subsequent sends return `BrokenPipe`.

Byte-marked control state for one direction lives in a `DirectionAncillary`
shared by the two endpoints: a `SpinNoPreempt`-guarded queue of pending
records plus the producer-written and consumer-consumed byte totals. The
producer takes the guard to register a record before its bytes are
published, so a consumer that observes the bytes always sees the record's
segment; the consumer takes the guard to compute its read cap, to deliver or
discard the reached record, and to advance its consumed total. Critical
sections are short and never wrap a socket operation, and the lock is never
acquired while holding both direction `tx_order` locks.

A blocking send that encounters shutdown after making progress returns the number of bytes sent. A nonblocking send also returns a partial count after making progress, and returns `WouldBlock` only when no data has been sent and the ring buffer is full. A zero-length send returns 0 when the connection exists and the corresponding directions remain open at both ends.

Bind records the Unix address, and `listen` enables connection reception. The listener queue is capped at `LISTEN_QUEUE_SIZE`; each request allocates its ring buffers only after reserving queue capacity. Shutting down the listener's read direction rejects new connections while preserving queued connections for `accept`.

### Unix Stream Byte-Marked Control Data

Unix stream `SCM_RIGHTS` is delivered by attaching a record to a byte range
(`[start, end)` of the sending `sendmsg`) and stopping each `recv` at the
reached record's segment end. Packet-mode control delivery cannot work on a
stream because the receiver has no message boundary to consume. This
matches Linux's `unix_stream_read_generic`, which ends the read after the
first fd-bearing skb and delivers one control batch per `recvmsg`; the
`scm_fp_dup` delivery point is reproduced by sharing each record's entries
behind one `Arc`.

`AncillaryData` is transported as `Arc<dyn Any + Send + Sync>` instead of
`Box<dyn Any + Send + Sync>` so a `MSG_PEEK` can duplicate an opaque control
payload without knowing its concrete type and without reopening the received
files. The peer's open file description stays shared because the receiver
installs the same `Arc<VfsFile>` reference into its descriptor table on
every delivery.

A no-control-buffer `recv` that reaches a record's segment drops the record.
This avoids the stale-inheritance bug class where a later `recvmsg` that
does provide a buffer silently receives control data from bytes the
receiver already consumed, and the loss is reported as `MSG_CTRUNC` like
Linux `scm_detach_fds`.

### Ethernet ARP Pending Queue

Ethernet devices use `pending_tx` to hold IP packets awaiting next-hop address resolution. Once the corresponding ARP reply arrives, the device sends waiting packets in queue order. This introduces head-of-line blocking: a next hop that remains unresolved blocks subsequent queued packets.

### vsock-TIPC Bridge

When `vsock_tipc_bridge` is enabled, `bridge` and AF_VSOCK share `VSOCK_CONN_MANAGER`. The connection manager owns access to the raw `VsockDevice`, and the bridge uses its interfaces to manage connections and data.

During initialization, `net/knet`'s `init_vsock()` obtains the raw virtio-vsock device from kclass and registers it with `VSOCK_CONN_MANAGER` through `connection_manager::register_vsock_dev()`. The manager maintains connection and listener tables, credits, and the bridge event queue, and polls the raw RX virtqueue through `poll_event()`. For ordinary ports, it performs accept and force_close operations, writes data into connection receive buffers, and updates credits. For bridge ports and connections, it queues `ConnectionRequest`, `Connected`, `Received`, `Disconnected`, and `CreditUpdate` events for `VsockBridge::rx_task` to consume through `pop_bridge_event()`.

AF_VSOCK accesses the manager through socket-facing helpers in `connection_manager`. `bridge::init()` registers ports 0–4 from `BRIDGE_PORT_MAP`, which the manager listens on while queueing bridge events. Bridge operations use `listen_bridge_port`, `create_bridge_connection`, `send_bridge`, `recv_bridge`, `disconnect_bridge`, and `abort_bridge` to listen, create connections, exchange data, and close connections.

Host-to-TA connections use the following port mapping:

| Port | TIPC service |
|---|---|
| 0 | Service name supplied through a dynamic handshake |
| 1 | `com.android.trusty.keymint` |
| 2 | `com.android.trusty.gatekeeper` |
| 3 | `com.android.trusty.vsock.forwarder` |
| 4 | `com.android.trusty.widevine.transact` |

#### Dynamic Handshake on Port 0

The Trusty-compatible dynamic bridge uses record semantics for the handshake and data forwarding after the host connects to vsock port 0.

1. The host sends the first vsock record containing a UTF-8 TIPC service path without a NUL terminator, limited to `IPC_PORT_PATH_MAX` bytes.
2. The guest bridge calls `ipc_port_connect_async(..., WAIT_FOR_PORT | ASYNC)` with that path. If the service has not been published, the channel remains `Connecting` and the handshake response waits until the TA publishes the service and TIPC becomes ready. An existing service also requires TIPC readiness before the bridge sends the single-byte status `[0]`. Both sides then forward data as records.
3. The guest sends `[1]` and closes vsock if service-name validation fails for UTF-8, length, or content; reading the service-name record fails; or the TIPC channel receives `HUP` or `ERROR` during `TipcConnecting`. These cases indicate connection rejection, such as an existing port refusing the connection.
4. After sending the service name, host-side `libtrusty` blocks waiting for the status byte. `TRUSTY_VSOCK_TIMEOUT_SEC` sets the receive timeout, which defaults to 60 seconds. A timeout or EOF indicates connection failure; `[1]` maps to `-EIO`.
5. If the service is never published, the guest continues waiting and the host eventually fails on receive timeout. The corresponding negative test expects this timeout.

Fixed ports 1–4 connect to their predefined TIPC services using `WAIT_FOR_PORT | ASYNC` when the vsock connection is established, without the single-byte handshake.

In the TA-to-host direction, the bridge publishes the `com.android.trusty.vsock.forwarder` TIPC port and connects accepted channels to host CID 2, port 0. `connection_manager::create_bridge_connection()` obtains an ephemeral port through the manager's `allocate_port()` and initiates the vsock connection; `accept_reverse_tipc()` then inserts a `BridgeConnection`. Ephemeral ports lie outside `BRIDGE_PORT_MAP`, and the manager routes lifecycle events to the bridge queue. On receiving `Connected`, the bridge adds the channel to the handle set.

The bridge processes `Received(conn_id, len)` through `recv_exact_record`, which rejects lengths above `IPC_CHAN_MAX_BUF_SIZE`, allocates a buffer of length `len`, and repeatedly calls `recv_bridge` until the record is complete. A zero-byte read before completion is an error. The completed record is sent as one TIPC message. If TIPC send returns `WouldBlock`, the bridge retains that single pending record and retries after `SEND_UNBLOCKED`.

Data flows from TIPC to vsock through `get_msg -> read_msg -> put_msg -> send_bridge`. When vsock credits are exhausted, `send_bridge` blocks on `tx_wait_queue` until `CreditUpdate`. Connection closure returns an error for the caller to handle. Bridge v1 forwards only bytes; TIPC handles and memory references are outside its scope.

With `vsock_tipc_bridge` enabled, `bind()` rejects explicit binding to ports in `BRIDGE_PORT_MAP`, and ephemeral port allocation skips these reserved ports.

## Supported Scope and Limitations

| Area | Support and limits |
|---|---|
| Internet / UDP | Configuration and native UDP support IPv4 only; IPv6 UDP endpoints return `EAFNOSUPPORT`. smoltcp IPv6 support does not extend these interfaces. |
| UDP ephemeral binding | Port-zero bind and automatic binding on first connect or send select randomized candidates in `49152–65535` using kernel entropy. If entropy is not ready, both blocking and nonblocking sockets return `EAGAIN` and remain unbound. Explicit nonzero-port binding retains its existing behavior. |
| Device binding | UDP demultiplexing checks the bound device. TCP/raw bound-device packet filtering and complete IPv6 scope semantics remain separate compatibility work. |
| DNS | smoltcp DNS support is compiled in; knet exposes no public resolver API. |
| AF_PACKET | Ethernet raw/datagram sockets with byte- and frame-bounded queues; filters `PACKET_OTHERHOST`. Membership changes, connect, peer queries, shutdown, listen, and accept are unsupported. |
| rtnetlink operations | Supports link queries/updates, IPv4 address and route queries/add/delete, and IPv4 neighbor updates. Other message types return `NLMSG_ERROR` with `EOPNOTSUPP`; neighbors accept only incomplete/permanent states with no flags. |
| Netlink sockets | Route sockets reject multicast subscriptions, userspace-peer destinations, and mixed query/mutation batches. Kobject-uevent sockets support group delivery, `SO_PASSCRED`, and validated no-op classic-BPF attach/detach compatibility. Connect, peer queries, and shutdown are unsupported. |
| Socket options | Options unhandled by the selected protocol return `ENOPROTOOPT`. |
| Unix datagrams | Receive queues use unbounded `async_channel` queues. |
| Unix stream control data | Byte-marked `SCM_RIGHTS` delivery with the segment-end barrier and `MSG_PEEK` duplication. Each `recvmsg` consumes at most one control record. Zero-length `sendmsg` with ancillary drops the control data because there is no byte to attach it to. Pending-record retention is bounded by `MAX_PENDING_RECORDS` / `MAX_PENDING_ANCILLARY`; a send beyond the quota fails with `ENOBUFS`. |
| Unix stream `MSG_PEEK` | `MSG_PEEK` preserves the byte stream and duplicates control data (shared open file descriptions). It still stops at the reached record's segment end, so a peek limited by that barrier returns fewer bytes than the destination buffer size. |
| Network namespaces | Socket, route, interface, and netlink tables are global, not per-network-namespace. `kns::NetNamespace` is a placeholder id, and `CLONE_NEWNET` / `setns` are not wired into `knet`. Pathname sockets already resolve through KVFS per operation, so cross-mount-namespace path views are consistent with the active mount namespace even though the binding registry itself is global. per-process netns isolation is tracked in the namespace subsystem, not here. |
| vsock / TIPC bridge | Stream transport only; the bridge forwards bytes, excluding TIPC handles and memory references. |
| Ethernet progress | Failed RX-scheduler attachment falls back to timeout polling. ARP pending traffic is subject to head-of-line blocking. |
| Device attachment | Initialization attaches at most one NIC. vsock accepts one raw transport; a second registration returns `AlreadyExists`. |

Implementation details: [packet.rs](../src/link/packet.rs), [rtnetlink.rs](../src/netlink/rtnetlink.rs), and [netlink/socket.rs](../src/netlink/socket.rs).

## Drop and Resource Release

| Resource / trigger | Cleanup and remaining state |
|---|---|
| IPv4 filter table drop | Releases owned chains, rules, and match values. Construction scratch storage is temporary. |
| TCP file close | Removes a `Closed` handle immediately when no reset is pending. Unread RX data causes abort, with the handle retained until RST is sent. Other handles remain in deferred close while the poller completes transmission and closure. Detached connections receive a 60-second reclamation deadline only in `FIN_WAIT_2`. |
| TCP listener close | Marks the listener closed, drains child handles, and removes them from `SOCKET_SET`. |
| Unix stream listener drop | Closes the listener queue, wakes waiters, clears the binding slot, and releases pending requests and ring buffers. |
| Unix stream shutdown / channel drop | Publishes directional closure and wakes affected waiters; channel drop closes both directions before releasing buffers. Pending byte-marked control records are dropped with the channel (received file references are released; no descriptor is installed). Peer recv drains published data before EOF, and poll reports `RDHUP`. Discarded unread data records one peer `ConnectionReset`, consumed by recv or `SO_ERROR`. |
| UDP socket / PCB drop | Unregisters the socket from the PCB registry; PCB destruction releases receive and asynchronous-error queues. |
| Raw socket drop | Shuts down both directions and removes the smoltcp handle. |
| Packet socket drop | Releases queues and decrements the active-handler count. Later registration or publication prunes expired weak entries. |
| Netlink socket drop | Releases receive state with the last strong reference. Binding updates or event publication prune weak uevent subscriptions. |
| Unix datagram drop | Clears the binding slot, releases the peer channel, and wakes peer waiters. |
| Unix pathname socket close | Releases the transport binding; the inode remains under VFS ownership. |
| Ethernet RX completion | Returns the frame buffer through the driver's `recycle_rx` interface. |
| Ethernet device removal | Detaches the RX scheduler and unregisters its RX source. `unregister_netdev` removes Router routes, addresses, and device state under `network_config_lock`. |
| vsock transport removal | Clears device/transport state, closes tracked connections, wakes connection/listener/credit waiters, clears listen and bridge-event queues, and resets polling references. Existing socket references retain connection entries until cleanup. |
| vsock stream drop | Shuts down the connection and removes its manager entry. |
| Bridge connection close | Removes the bridge entry and TIPC handle-set registration, closes the channel, and disconnects vsock. Disconnect/abort wakes credit waiters before removing the manager entry. Reverse TIPC ports remain registered for the global bridge runtime's lifetime. |
