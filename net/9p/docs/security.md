# p9 — Security and Reliability Analysis

## Transport owner admission

Transport storage failure is handled by the caller before constructing a client
or admitting remote fids. P9Client owns an already initialized KBox and does not
introduce a second owner-allocation failure after admission. Existing tests use
prepared transport fixtures and retain malformed-response and fid-cleanup
coverage; they do not force global OOM or make nested buffers fallible.


## Trust Model

The transport peer and all response PDUs are untrusted. A qid, directory entry, attribute, string, errno, or file content may be exposed to a filesystem caller only after it has
passed wire validation. The kernel caller, the established transport
connection, and the allocator/scheduler are considered trusted.

## External Boundaries / Attack Surface

- Name components, symlink targets, open flags, offsets, and I/O lengths supplied by the caller;
- PDU headers, payloads, nested lengths, and server errnos written by the transport;
- Endpoint disconnection, short responses, error responses, or a permanent lack of response;
- Synchronous external I/O triggered by the Drop of the final fid.

This crate does not access user pointers, MMIO/PIO, DMA buffers, firmware, FFI, or architecture raw interfaces. How a concrete
transport handles these boundaries is outside the scope of `p9`.

## unsafe Code Inventory

This crate has no `unsafe` blocks, unsafe APIs, or custom `Send`/`Sync` implementations.

## Memory Safety Invariants

- The transport length, the PDU declared size, and field boundaries are validated before response slicing;
- Nested lengths in read/readdir must not extend past the payload, and must not exceed the request count;
- The number of bytes completed in a write response must not exceed the length of data actually sent in this request;
- For `P9Transport::request`, a length that exceeds the buffer, even if reported as an error, is rejected before slicing;
- `P9Fid` holds an `Arc<P9Client>`, so the transport is never released before the request or the final clunk;
- Every live fid number is unique within a client, and may be reused only after clunk/remove.

## Thread Safety

The client's sleepable mutex serializes transport requests, the dialect/msize, and the tag/fid allocator. A fid's
qid uses a mutex, while the open mode, iounit, and live state use atomics. The lock order is always client state
first, then the fid qid; no reverse path exists. The state checks for open and I/O, the iounit read, and the request send all complete in the same
client critical section, and do not rely on an additional opening flag. All of these locks may be used only in a sleepable task context.

## Threat Analysis

| Threat | Trigger Condition | Impact | Mitigation |
| --- | --- | --- | --- |
| Out-of-bounds parsing | Short packet or forged declared/string/data length | kernel memory safety | Checked decoder and nested length validation |
| Response confusion | Peer returns the wrong type/tag | Another response is treated as the result of this request | Every request validates the exact type/tag |
| Partial walk | `RWALK` returns fewer qids than names | Publishes a wrong remote object or leaks a fid | Reject the result and best-effort clunk the new fid |
| Fid double release | Object still has a Rust reference after remove | Server handle is clunked twice | The consumed live state is set as soon as remove is committed |
| Cross-connection operation | The fids of link/rename come from different clients | Request references an unrelated server handle | Compare client identity before sending and return `EXDEV` |

## Failure Mode and Effects Analysis (FMEA)

| Failure Mode | Local Effect | System Effect | Handling |
| --- | --- | --- | --- |
| dialect/msize unsupported | Negotiation fails | The mount is not published | Typed error and release the client |
| malformed PDU | The current operation fails | Object state is not committed | `InvalidData` |
| server `RLERROR` | The current operation fails | The caller receives the corresponding errno | Preserve the numeric errno |
| final clunk fails | The server fid lingers temporarily | Endpoint resources are reclaimed only when the connection is dropped | Warning; the local handle is marked released |
| Endpoint permanently unresponsive | The request task blocks | Operations on the same client are queued | Currently no timeout/cancel |

## Fault Management

Protocol and transport failures are propagated through `KResult`. Only Drop cannot return a clunk error, so it logs a
warning; a failure is never treated as a successful filesystem operation. Partial walk and open failure respectively clean up the
new fid and keep the unopened state.

## Privacy Analysis

File names, metadata, symlink targets, and file contents are sent to the configured endpoint. This layer does not provide transport encryption;
confidentiality depends on the selected transport, the VMM/host, and the deployment isolation policy.

## Known Limitations

- Only 9P2000.L is negotiated;
- The requests of one connection are synchronously serialized;
- There is no timeout, cancel, legacy/dotu, xattr, or remote lock API.

## Audit Checklist

- Does a new PDU validate the type, tag, fields, and nested lengths;
- Does a new operation preserve the Linux fid roles and the wire field order;
- Can a fid be reused while live, or clunked again after remove;
- Has an absolute-path API been reintroduced, or has filesystem state been placed into protocol objects;
- Does a new field correspond to necessary state for Linux semantics or Rust ownership/synchronization.
