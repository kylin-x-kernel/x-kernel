# Vsock Contract Security And Reliability

## Trust Model

The contract crate trusts callers to obey buffer ownership rules and trusts
drivers to fill protocol headers correctly. It validates buffer geometry and
frame lengths at the API boundary. It does not trust an upper layer to stay
within advertised credit; each transport must cap submitted payloads again.

## TX Buffer Safety

`VsockTxBuf` accesses pool storage through `get_slice()` and `get_slice_mut()`.

**Invariants:**

- A slot is owned by exactly one `VsockTxBuf` between allocation and drop.
- The pool backing `Vec<u8>` is allocated once and never resized.
- `hdr_len + payload_len` never exceeds `buf_len`.
- All slice starts and lengths are checked against the owned slot.
- `&mut self` is required for mutable slice access.

**Mitigation:** Setters validate the combined frame length before mutating
metadata. Slice construction uses the validated lengths and the stable pool base
pointer captured from `Vec::as_mut_ptr()` while the storage is uniquely owned in
the pool constructor, so writes never go through a pointer derived from a shared
`Vec` reference. Dropping a buffer returns its offset to the pool exactly once.

## Failure Modes

| Failure | Effect | Mitigation |
| --- | --- | --- |
| Pool allocation fails | Driver initialization fails | `try_reserve_exact` maps failure to `DriverError::NoMemory`. |
| Pool exhausted | No TX buffer available | `alloc_buf()` returns `None`; drivers map this to `WouldBlock`. |
| Frame length overflow | Invalid buffer access | Header/payload setters return `DriverError::InvalidInput`. |
| Caller drops an unsent buffer | Slot returns to the pool | The driver free list may observe a temporary capacity loss; callers must submit or explicitly return allocated buffers. |
| Peer credit is exhausted | No data can be submitted | `VsockDevice::request_credit()` asks the peer for an update; the manager waits for capacity. |
