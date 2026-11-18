# Vsock Contract Design

## Scope

The `vsock` contract crate defines transport-neutral data types and commands
shared by vsock drivers and their consumers. It does not own connection tables,
listen state, receive buffering, virtqueue descriptor tokens, or wire-format
encoding.

## Address And Connection Types

`VsockAddr`, `VsockConnId`, and `VsockConnectionInfo` describe connection
identity and credit state. `VsockConnectionInfo` is snapshotted by the connection
manager. A transport reads it to encode packet headers and must not mutate it.

`peer_free()` computes the peer's remaining receive window as
`saturating(peer_buf_alloc - (tx_cnt - peer_fwd_cnt))`. The subtraction is
wrapping to preserve the protocol's 32-bit counter arithmetic; the final window
is saturated at zero.

## TX Buffers

`VsockTxBuf` and `VsockTxBufPool` provide a fixed-size buffer abstraction for
transmit paths.

- `VsockTxBufPool::new(slot_count, buf_len)` allocates one contiguous backing
  store and tracks free offsets.
- `VsockTxBufPool::alloc_buf()` returns an owned `VsockTxBuf`, or `None` when
  all slots are in use.
- Dropping a `VsockTxBuf` returns its slot to the pool.
- The free-slot list is protected by a `SpinNoPreempt` lock. Allocation and
  release run in task context; a driver that reclaims TX buffers from IRQ
  context must defer the release to task context.
- `VsockTxBuf` tracks header length, payload length, and capacity. Header and
  payload setters reject frames larger than the slot.
- `header()`, `payload()`, `frame()`, and `buffer()` expose the three regions
  used by transports: protocol header, payload, and the whole slot.

The pool does not know a protocol header size. A driver sets the header length
before exposing a buffer to the upper layer, so `payload_capacity()` reflects
the space remaining after the driver header.

## TX Commands

`VsockDevice` exposes asynchronous TX commands:

1. `alloc_tx_buf()` obtains a transport-owned buffer with reserved header
   headroom.
2. The caller fills `payload_mut()` and calls `set_payload_len()`.
3. `submit_tx_buf()` transfers ownership to the transport, encodes the header,
   and submits without waiting for completion. Submission is all-or-nothing:
   insufficient credit or transport capacity returns `WouldBlock` without
   accepting any payload bytes.
4. `recycle_tx()` reclaims completed descriptors and returns buffers to the
   transport free list.
5. `request_credit()` sends a peer credit request when the peer window is
   empty.
6. `can_send()` reports whether the transport currently has TX capacity, so
   callers can build wait predicates without first allocating a buffer.

A transport retains submitted buffers until device completion. The connection
manager owns all connection and credit policy; the transport only enforces
transport capacity and the protocol packet-size limit.

## Relationship To Net Buffers

The abstraction mirrors the network contract's `NetBuf`/`NetBufPool` ownership
model. Network buffers include a general link-layer frame and cross the
`NetDevice` contract through `NetBufHandle`. Vsock TX buffers use the same
header/payload/frame layout but are returned directly as RAII `VsockTxBuf`
values because the current vsock contract does not need a raw handle. Virtqueue
token bookkeeping remains in the concrete driver.
