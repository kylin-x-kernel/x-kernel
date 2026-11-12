# p9 — Design Document

## Prepared transport ownership

P9Client consumes a caller-prepared KBox<dyn P9Transport> and transfers it into
client state without reboxing. Preparation precedes protocol locking and
negotiation. Client Arc creation, request buffers and live-fid collections retain
their existing allocation behavior. Request serialization and final transport
destruction remain unchanged.


Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.
Enable `KFEAT_FS_9P=y` in the kernel configuration to include the `p9` tests.

## Scope and Role

`net/9p` corresponds to Linux `net/9p`. It owns the transport-independent 9P2000.L client, fids, and
PDU encoding/decoding. It does not implement a VFS filesystem and is not responsible for device discovery; the concrete transport connection is established by the caller and then
handed to `P9Client`.

| X-Kernel | Linux | Responsibility |
| --- | --- | --- |
| `client.rs` | `net/9p/client.c`, `client.h` | `P9Client`, `P9Fid`, and protocol operations |
| `message.rs`, `protocol.rs` | `net/9p/protocol.c`, `9p.h` | PDU encoding/decoding, qid, and protocol constants |
| `transport.rs` | connection operations of `net/9p/transport.h` | Request interface of a single connected endpoint |

`fs/filesystems/9p` corresponds to Linux `fs/9p` and only adapts these protocol objects to KVFS.

## Background

A numeric fid is only a server handle; it cannot express client ownership, qid, open state, and the final clunk.
This module encodes Linux's client/fid reference relationship directly as Rust objects, so that the filesystem layer no longer maintains path
strings or raw fid lifetimes.

## Boundaries

The client, wire protocol, and connection trait listed in the table above belong to this module; transport discovery, the concrete
VirtIO driver, and the KVFS adapter do not belong to this module.

## Architecture

```text
Arc<P9Fid> ──owns──► Arc<P9Client>
    │                    │
    │ object methods     └── Mutex<P9ClientState>
    │                         ├── msize / dialect
    │                         ├── tag / live-fid allocator
    │                         └── KBox<dyn P9Transport>
    └── fid / qid / open mode / iounit / live state
```

`P9Fid` is the Rust object representation of Linux `struct p9_fid`. `Arc` directly serves as the fid reference count, and the fid
holds the client reference, the server-side number, the qid, the open mode, the iounit, and whether it is still valid. Fields that current Linux semantics do not use
and that cannot affect object behavior are not copied into the Rust structure.

`P9Client` corresponds to the protocol ownership of Linux `struct p9_client`. An internal sleepable mutex serializes requests on a synchronous
transport connection as well as tag/fid allocation, so upper layers do not need to create a session-level outer lock.

## State Machine

```text
attach / complete walk
          │
          v
      unopened ──TLOPEN──► opened
          │                   │
          ├──TREMOVE (from unopened or opened)──► consumed
          └──── final Arc drop ┴──► TCLUNK ──► released
```

- A zero-name `walk()` creates an independent fid for the same remote object; a non-empty walk accepts only a single protocol name component;
- A partial walk does not publish a new object, and a fid already established by the server is clunked;
- open checks unopened, sends the request, and commits the result inside the client mutex; on failure the fid stays unopened, and no opening state is added;
- After `TREMOVE` is committed, the fid becomes invalid per Linux semantics, and no second clunk is sent even if the request returns an error;
- When the last `Arc<P9Fid>` is released, `TCLUNK` is sent synchronously, and the client/transport is kept alive until it completes.

## Algorithm Flow

`P9Client::new()` takes over a connected transport. `negotiate()` negotiates only the 9P2000.L that the current filesystem adapter
actually implements, records the smaller of the two msize values, allocates the root fid, and performs attach.

The fid object directly provides walk, open, getattr/setattr, read/write/readdir, fsync, statfs, and namespace
operations. The filesystem layer does not pass absolute paths and cannot operate on numeric fids directly. The amount of data in a single read, write, or readdir
is limited by both the negotiated maxdata and the server iounit.

Every synchronous request verifies that:

- the length returned by the transport does not exceed the response buffer;
- the length declared in the PDU matches the actual length and includes at least the 7-byte header;
- the response tag and type match the current request;
- all integers, qids, strings, and embedded data lengths are within the packet boundary;
- the numeric errno of a 9P2000.L `RLERROR` is converted directly to `KError`.

The fid allocator tracks the numbers that are still live, and on wraparound it does not reuse a server handle that has not yet been clunked/removed; this corresponds to the
Linux client's fid IDR, rather than additional filesystem state.

## Calling Constraints / Execution Context

Protocol calls and the Drop of a live fid may both block, so they may only run in a sleepable task
context after the scheduler and allocator are available; a current userspace process is not required, but the last fid reference must not be
released from IRQ context or from a path that holds a spinlock. The synchronous client connection is not reentrant; reentrant and concurrent requests are
serialized by the client mutex.

## Concurrency Model

The client mutex protects the negotiated state, the tag/fid allocator, and the mutable transport request all together.
The qid uses a mutex, while the open mode, iounit, and live lifecycle use atomic state, so that checks on a shared fid stay consistent with its final
release. The open state check, the response commit, and the iounit limit calculation for I/O are all in the same client
critical section, which avoids using a stale I/O limit while waiting for open.

## Design Decisions

- 9P is a protocol that can be carried over transports such as VirtIO, TCP, and fd, so the client/fid live in `net/9p`;
- `P9Transport` represents only a single connection; this change does not introduce a transport registry or move drivers;
- Transport selection and VirtIO device claim remain in the existing boot/driver glue;
- The API is expressed in terms of client/fid objects, and no path-based convenience API is kept;
- Only synchronous 9P2000.L is currently supported, and legacy/dotu, cache, xattr, or lock semantics that are not yet implemented are not declared.

## Drop / Resource Release

The last `P9Fid` reference synchronously sends `TCLUNK` and releases the number from the client's live-fid set; a consumed fid for which
`TREMOVE` has already been committed is not clunked again. A fid holds the client until cleanup completes, and the transport connection is released only after the last client/fid
reference is released.
