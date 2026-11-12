# tipc — Design Document

## Positioning

`tipc` is the transport-independent core crate of Trusty IPC in X-Kernel.
It maintains ports, channels, messages, memrefs, and the global service registry.

`tipc-handle` provides the `Handle` trait, `HandleSet`, and the process-local `HandleTable`,
which `tipc` and `process/kprocess` both depend on.
`tipc` also hosts the Trusty syscall ABI adapter;
`core/ksyscall` only forwards TIPC syscall numbers to this adapter in the overall syscall dispatch.

## Background

Trusty TIPC establishes bidirectional channels over named ports, and passes handles as object capabilities.
X-Kernel first needs an IPC core that does not depend on a specific TEE runtime or syscall ABI, so that later syscall adaptation, TA services, and kernel-side helpers can share the same state machine.

The current implementation prioritizes the following semantics:

- service port creation, publication, connection, and accept;
- a client may enter the waiting state before the port is published;
- channel with two endpoints, a fixed-slot message queue, and message boundaries;
- handle events, cookies, handle sets, and the process-local handle table are maintained uniformly by `tipc-handle`;
- the basic ownership model for handle passing;
- Trusty-visible metadata of memref handles and validation of user mappings at creation time.

## Scope

Source files involved:

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

Related shared handle crate:

```text
tee/tipc-handle/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── handle.rs
    ├── handle_set.rs
    └── handle_table.rs
```

## Architecture

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

| Component | Responsibility |
|------|------|
| `IpcPort` | Represents a named server-side port; holds the publication state, access policy, receive queue configuration, and the queue of server endpoints not yet accepted |
| `IpcChan` | Represents one end of a bidirectional connection; holds the lifecycle state, a weak reference to the peer, the receive queue, the event state, and the cookie |
| `IpcMsgQueue` | Fixed-slot message queue; preserves Trusty message boundaries and the three-phase `get/read/put` read model |
| `PortRegistry` | Global service namespace; looks up published ports by path and holds client endpoints waiting for a port to be published |
| `tipc_handle::Handle` | Unified interface for events, close, cookie, and downcast for ports, channels, handle sets, and memrefs |
| `tipc_handle::HandleSet` | Aggregator of events from multiple handles; returns ready events by process-local handle id |
| `tipc_handle::HandleTable` | Mapping from process-local integer id to `Arc<dyn Handle>`; responsible for detaching from handle sets on close |
| `MemRef` | Metadata of a memory reference that can be passed through messages; the current process mapping is validated at syscall creation time, and it is currently not bound to an mmap-able VMM object |
| `syscall` | Trusty syscall ABI adapter; handles user pointers, syscall arguments, and attachment to the current process handle table |

## Calling Constraints / Execution Context

- This crate is a `no_std` kernel crate and depends on `alloc`, `kspin`, `kpoll`, and `ktask`.
- The API targets task/syscall lifecycle paths and must not be called in interrupt context.
- `IpcChan::wait_connected` waits for the connection to complete via `ktask::future::block_on`, and the internal
  `PollRegistrations` is held across `Pending`; the caller must be in a context that allows blocking.
  This wait can be interrupted by a fatal signal, in which case it returns `KError::Interrupted`.
- `tipc_handle::Handle::register` uses `PollContext` and may return a registration error; poll/wait
  callers must run after the scheduler is available, and must refresh the registration context on every poll round.
- `tipc_handle::HandleTable` itself does not hold a lock; the caller is responsible for placing it under a process-local lock.
- The `syscall` module depends on `kprocess`, `khal`, `linux_sysno`, and `posix-types`, and may only be used in the user-thread syscall context.
- `PortRegistry` is a global namespace; path, port publication, and waiting-client operations are serialized by an internal `SpinNoIrq`.
- TIPC ports or channels must not be created during early boot; this module assumes that the allocator, scheduler, and basic synchronization facilities are already available.

## State Machines

### Port Lifecycle

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

After creation, an `IpcPort` is first in `Invalid`, and enters `Listening` once published successfully.
Closing a port removes it from the global registry and closes all pending server endpoints.

### Channel Lifecycle

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

A client endpoint can be created before the target port.
Only when the port has been published, or is published later, does `port_attach_client` allocate the receive queues on both sides and create the server endpoint.

### Message Slot State

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

`read` does not release the slot; only `put` clears the data and the attached handles and returns the slot to the free list.

### Handle Table Lifecycle

```text
Arc<dyn Handle>
  └─ uctx_handle_install()
      ▼
process-local integer id
  ├─ uctx_handle_get()
  ├─ wait_any_snapshot()
  ├─ register_wait_any_table_change()
  ├─ uctx_handle_remove()
  │    ├─ detach_from_handle_sets(id)
  │    └─ handle.close()
  └─ uctx_handle_close_all() during exec / last-thread exit
       └─ close every handle
```

A handle id is a process-local capability, not a global id.
When a handle is closed, `HandleTable` removes that id from all `HandleSet`s in the same table, so that a stale registration does not keep a closed object alive.
Both at the exec boundary and when the last thread of a process exits (before process exit is published), `uctx_handle_close_all` is invoked via
`Process::close_all_tipc_handles`,
so that when the parent's `wait` returns or on device shutdown, an exited process is never observed still holding a live
port/channel owner.
`sys_tipc_wait_any` uses the wait-any snapshot cached by `HandleTable`;
the cache is rebuilt only after handles are added to or removed from the handle table, avoiding allocating a `Vec` and cloning
all `Arc<dyn Handle>` on every poll round. The wait path registers for both handle events and handle-table
membership changes, to prevent a missed wakeup when a handle is installed or closed.

## Algorithm Flow

### Creating and Publishing a Port

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

Waiting clients are attached one by one after the registry is unlocked, to avoid performing the more complex channel creation and port pending-list operations while holding the global registry lock.
The port-local critical section inside `IpcPort` rechecks the `Listening` state and serializes the client/server binding with the pending queue update, so a concurrent close cannot slip in between the state check and the pending enqueue.

### Connecting to a Port

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

A synchronous connect is completed by the syscall adapter or the caller invoking `wait_connected` after the client endpoint is returned.

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

### Sending and receiving messages

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

The current implementation forbids adding a `HandleSet` to another `HandleSet`.
This is more conservative than Trusty, which allows acyclic nesting, and avoids introducing cycle detection in the first phase.

## Concurrency Model

- `PortRegistry` uses a global `SpinNoIrq<PortRegistryState>` to protect the path registry and the waiting client list.
- `IpcPort::inner` uses `SpinNoIrq` to protect both the port lifecycle state and the pending server endpoint queue, so that the state check and the queue update are not separated between attach/accept/close.
- `IpcChan::peer` and `msg_queue` use `SpinNoIrq`; the lifecycle state and auxiliary events use atomics.
- `IpcMsgQueue` has no lock of its own and is accessed only while holding the `IpcChan::msg_queue` lock.
- `HandleSet::entries` uses `SpinNoIrq<BTreeMap<...>>`, which serializes registration, removal, and poll.
- `HandleWaitState` uses `kpoll::PollSet` to hold waiters and an `AtomicUsize` to hold the cookie.
- `HandleTable` has no built-in lock; process state typically wraps it in an `RwLock<HandleTable>`.

The lock granularity favors simplicity and locality.
The global registry lock protects only the namespace; channel and message operations do not hold the registry lock.

## Design Decisions

- **The syscall adapter follows the TIPC owner**: The Trusty IPC syscall ABI lives in the `tipc` crate, which makes it easier to maintain the TIPC-specific ABI, UUID conversion, and the core state machine together; `core/ksyscall` keeps only the top-level dispatch table.
- **Handle ownership is split out into `tipc-handle`**: `process/kprocess` only needs a process-local handle table and should not depend on the full TIPC core; `tipc` and `kprocess` both depend on `tipc-handle` to avoid a circular dependency.
- **The registry holds weak ports**: The port lifetime is owned by strong references from the creator's handle table, and the global namespace does not prevent the object from being released.
- **A client can wait for an unpublished port**: `WAIT_FOR_PORT` is implemented by storing a weak client endpoint; if the client is closed before the port is published, the stale weak reference is skipped at publish time.
- **Fixed-slot message queue**: It preserves Trusty message boundary and back-pressure semantics; when the queue is full it returns `WouldBlock`, and the sender is notified once a slot is released.
- **Handle sets cannot be nested**: This avoids the complexity of cycle detection in the first phase; if alignment with Trusty is needed later, it can be extended to acyclic nesting.
- **memref is currently a metadata handle**: `sys_tipc_memref_create` verifies that the page-aligned address range is covered by the current process's VMAs and has the requested READ/WRITE user permissions; the handle currently stores the validated address, length, and permission bits, and does not establish a cross-address-space mapping.
- **memref mmap semantics are validated first and wired up later**: `MemRef::validate_mmap` checks, following Trusty rules, whether the offset, size, and requested permissions fall within the memref's capabilities; the actual VMM object binding and the mapping into the target address space are still to be wired up.

## In-Kernel Callers

The port, channel, and handle set objects of the TIPC core serve both the syscall adapter and in-kernel callers.
The vsock-TIPC bridge in `knet` does not go through the process handle table, and instead directly reuses the following object APIs:

- `ipc_port_connect_async(..., WAIT_FOR_PORT | ASYNC)` creates a host-originated client channel.
- `ipc_port_create` + `ipc_port_publish` create the forwarder service that the bridge exposes to the TA.
- `IpcPort::ipc_port_accept` accepts a TA-originated channel without blocking.
- `IpcChan::ipc_send_msg`, `ipc_get_msg`, `ipc_read_msg`, and `ipc_put_msg` forward message bytes while preserving Trusty message boundaries.
- `HandleSet::handle_set_create`, `handle_set_ctrl`, and `poll_one` let the bridge worker wait for READY, MSG, SEND_UNBLOCKED, HUP, and ERROR.

The bridge v1 forwards message bytes only.
If a TIPC message carries a handle or memref, the bridge treats the message as unsupported and closes the connection;
forwarding object capabilities across vsock requires a separate capability model and lifecycle design.

## Drop / Resource Release

- `IpcPort::drop` calls `close`, removes itself from the registry, and closes pending channels.
- `IpcChan::drop` calls `close`, which moves the channel to `Disconnecting`, also marks the peer `Disconnecting`, and notifies both endpoints; `poll` then reports HUP. `close` does not clear the receive queue.
- `HandleTable::uctx_handle_remove` first removes the target id from all handle sets, and then calls `close` on the target handle.
- `IpcMsgQueue::put` clears the slot data and attached handles, releasing the strong references to the transferred handles.
- `MemRef::close` currently only wakes waiters; the actual unbinding of VMM resources is not yet implemented.
