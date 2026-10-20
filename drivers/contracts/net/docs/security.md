# net — Security And Reliability

## Scope

This analysis covers the entire `net` contract crate: `src/lib.rs` (the
`NetDevice` / `NetRxScheduler` contracts and re-exports) and
`src/net_buf.rs` (the `NetBuf` pool family). The dormant `src/fxmac.rs`
and `src/ixgbe.rs` adapters are not compiled into the crate and are
excluded; they contain no reachable code in this configuration.

Guest test bodies live in `ut-tests/` and retain their original source-module
ownership through `unittest::path_mod_test!`. Only `ut-tests/net_buf.rs`
(two cases) is reachable under `cfg(unittest)`; the two cases each in
`ut-tests/fxmac.rs` and `ut-tests/ixgbe.rs` remain excluded with their dormant
parents. No hardware, SDK dependency, or public API is enabled by relocation.


## Trust Model

The crate sits between two kernel-side parties: NIC drivers implement the
contracts, and the network stack consumes them. It never touches user
memory, devices, or firmware directly, so it adds no trust boundary of its
own — it hands raw buffers and raw pointers across the boundary and
documents the obligations each side must uphold.

## External Boundaries

- `NetBufHandle` carries raw pointers (`owner_ptr`, `data_ptr`, `data_len`)
  across driver interfaces. Drivers receive these pointers and may only
  round-trip them back through `NetBuf::from_handle` / `recycle_rx`.
- `NetBufPool` storage is kernel heap memory (`Vec<u8>`), never
  device-owned memory; devices write into it only during driver-managed
  DMA.
- No FFI, no inline assembly, no MMIO access: hardware knowledge stays in
  driver crates.

## Unsafe Code

All unsafe code lives in `src/net_buf.rs` (the dormant `fxmac.rs` /
`ixgbe.rs` adapters are not compiled in):

- `NetBuf::get_slice` / `get_slice_mut` — form byte slices at
  `base_ptr().add(start)` inside the pool slot. Safety: `start + len` is
  checked against the slot's `buf_len` (checked arithmetic plus
  `debug_assert`), and `&mut self` excludes concurrent mutable access.
- `NetBufHandle::data` / `data_mut` — dereference the handle's
  `data_ptr` for `data_len` bytes. Safety: the handle was created by
  `into_handle` from a live `NetBuf`, so the pointer range is inside the
  pool allocation; `&mut self` serializes access.
- `NetBuf::from_handle` (`pub unsafe fn`) — `Box::from_raw` on the
  handle's owner pointer. The `# Safety` contract: the handle must have
  been produced by `into_handle` from a live allocation and consumed
  exactly once; forged, stale, or double-consumed handles are undefined
  behavior. This is the one boundary a compromised driver could violate;
  drivers are trusted kernel components.

## Invariants

- Every `NetBuf` slot offset is unique among live buffers; the pool's free
  list never hands out the same offset twice before release.
- A slot's contents are exclusively owned by its `NetBuf`; mutable access
  requires `&mut self`.
- Frame lengths stay within the slot: `set_hdr_len` / `set_payload_len`
  reject header+payload overflow (`DriverError::InvalidInput`).
- Buffer lifetime is RAII: `Drop` returns the offset to the pool, so a
  dropped buffer is immediately reusable but never aliased.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Forged or double-consumed `NetBufHandle` | High — UB via `Box::from_raw` | A driver fabricates a handle or recycles one twice | Documented single-consumption `# Safety` contract on `from_handle`; drivers are in-crate trusted code. Residual risk accepted by the driver trust model. |
| T-02 | Out-of-bounds header/payload sizing | Medium — out-of-bounds slice access | Caller sets `hdr_len + payload_len` beyond the slot | Checked arithmetic and `DriverError::InvalidInput` in `set_hdr_len`/`set_payload_len` before any slice is formed. |
| T-03 | Device writes past its buffer into the pool | High — corruption of neighboring slots | Mis-programmed DMA on the device side | Out of scope here: drivers size DMA descriptors from slot geometry; the pool keeps one slot per buffer to bound the blast radius. Residual risk sits with the driver/device boundary. |
| T-04 | Unbounded pool exhaustion | Low — allocation failure, no progress | All slots checked out and dropped buffers leaked | RAII drop returns slots deterministically; `alloc_buf` returns `None` instead of blocking or panicking. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Pool exhausted (`alloc_buf` → `None`) | Caller cannot allocate a buffer | Packet drop / backpressure at the caller | 3 | Callers retry or drop; no blocking in the pool. |
| F-02 | Invalid pool parameters in `NetBufPool::new` | `DriverError::InvalidInput` | Driver init fails, device not registered | 3 | Explicit validation of slot count and buffer length range. |
| F-03 | `set_rx_scheduler` on a poll-only driver | `DriverError::Unsupported` | Network stack falls back to polling | 4 | Default trait implementation documents the condition. |

## Known Limitations

- Buffer contents are not zeroed on allocation or recycle; drivers must
  treat buffer contents as uninitialized and must not leak stale kernel
  data into transmitted frames.
- The handle type cannot structurally prevent misuse; the safety argument
  rests on the documented driver-side contract.

## Test Coverage And Limits

The two reachable netbuf tests use local Vec-backed handles and local pools;
their backing allocations outlive handle access, and normally dropped buffers
return their slots. They exercise fixed boundary inputs in the kernel harness,
not real DMA or NIC interrupts. They do not prove safety for forged handles or
concurrent device writes.

The four dormant adapter cases are preserved historical mock tests, not
validated coverage. Their raw-pointer fixtures and SDK-dependent parent code
must be audited before any future enablement. In particular, the old fixtures
use a boxed Vec object's address as both owner and packet-data pointer; that
does not identify the Vec's backing byte allocation. Relocation preserves this
unreachable code and does not claim it is safe to execute.

## Audit Checklist

- Every `unsafe` block in `net_buf.rs` still matches the bounds described
  above (slot-checked, single-consumption).
- No new public API exposes raw pointers without a `# Safety` section.
- Pool slot geometry (offset, length) still passes the `InvalidInput`
  validation path.
