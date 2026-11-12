# vdev-virtio-blk — Security and Reliability Analysis

## Trust Model

- The caller (the VMM assembly code) is trusted: it provides `GuestDma`, `IrqSender`, and the capacity.
- The guest is untrusted: it controls the MMIO registers and the virtqueue contents, including the
  `request_type` and `sector` in the request header, as well as descriptor addresses and lengths.
- This crate must guarantee that a malicious request results at most in that request completing with IOERR, and must not access host memory out of bounds,
  must not panic, and must not corrupt device/VM state.

## External Boundaries / Attack Surface

| Boundary | Description |
|------|------|
| Standard MMIO registers | The transport state machine from `vdev-virtio` (including the `QUEUE_READY` layout validation) |
| Config space reads | Constants such as segment size and geometry, read-only |
| Request header descriptor | Guest-provided `VIRTIO_BLK_T_*`, `sector`, descriptor direction and length |
| Data descriptor chain | Guest-provided addresses/lengths, read and written via `GuestDma` |
| status descriptor | The device writes a 1-byte status |
| `QUEUE_NOTIFY` | Triggers queue processing |
| Backend | `BlockBackend` implementation (RAM disk by default) |

## unsafe Code Inventory

This crate contains no `unsafe` code. All guest memory accesses go through `vdev-virtio::DmaGuestMemory`
→ `vdev_core::GuestDma`; `unsafe` appears only in the `GuestDma` implementation in `kvmm` and in
`vdev-virtio::memory` (see their respective documents).

## Memory Safety Invariants

1. Data requests must pass `validate_data_request`: segment count ≤ `seg_max`, each segment ≤
   `size_max`, total length aligned to `SECTOR_SIZE`, and `[sector, sector+len)` within the capacity.
2. The descriptor direction must match the request type: `IN` requires the data descriptors to be writable, and `OUT` requires them to be readable.
3. The request header descriptor must be readable with length ≥ `VirtioBlockHeader::SIZE`; the status descriptor must be
   writable with length ≥ 1.
4. Buffer allocation uses `try_reserve_exact`; on allocation failure it returns
   `VirtioError::InvalidBufferSize`, and never calls `resize` directly on a guest-declared length, which could cause an OOM panic.
5. All `GuestDma` access failures are mapped to I/O errors and are not propagated as UB.

## Thread Safety

- The device instance is accessed serially as a `Box<dyn MmioDevice>` by the VMM MMIO bus via `&mut self`;
  internal shared state is protected by `SpinNoIrq`.
- `IrqSender` is `Send + Sync`; injection is invoked outside the device lock.
- The memory disk of `RamDiskBackend` is protected by `kspin::SpinNoIrq` and can be accessed safely when triggered from multiple vCPUs.
  Backend I/O runs inside the queue `SpinNoIrq` lock (with local interrupts masked), so the backend lock must be non-sleeping,
  to avoid a deadlock caused by blocking while interrupts are masked.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Over-capacity/out-of-bounds read/write request | High | Guest supplies an out-of-bounds `sector`/length | Double validation by `validate_data_request` + `RamDiskBackend::checked_range`; failure yields IOERR |
| T-02 | Forged descriptor direction | Medium | The data descriptor of an `IN` request is not writable, etc. | `process_read`/`process_write` validate the direction; failure yields `InvalidDescriptor` |
| T-03 | Oversized length causing memory exhaustion | Medium | Guest declares a huge data length | The request length must satisfy the descriptor and `seg_max`/`size_max` constraints; buffers use `try_reserve_exact` |
| T-04 | Illegal request header/status descriptor | Medium | Chain too short, header writable, status not writable | Validate chain length and direction before processing |
| T-05 | Processing a stale pending request after reset | Medium | Guest resets during a backend delay | Reset clears `pending_head` |
| T-06 | Deferred backend without polling stalls the queue | Low | `requires_deferred_processing` is true but the VMM does not poll | `deferred_pending` records it and logs a warning; the VMM must provide polling |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Request completes with IOERR | Illegal range/direction/backend error | Single request fails | Guest I/O reports an error, can be retried | 3 | Write back `VIRTIO_BLK_S_IOERR` and continue processing subsequent requests |
| F-02 | Request pending | Backend `WouldBlock` | The head is stored in `pending_head` | Queue is deferred until processing resumes | 3 | Return `QueuePending`; `process_pending_queue` continues it |
| F-03 | Buffer allocation failure | Out of memory | Request gets IOERR | Guest I/O fails | 3 | `try_reserve_exact` failure returns an error, no panic |
| F-04 | Unsupported request | Unknown `request_type` | Request completes with UNSUPP | Guest sees it as unsupported | 4 | Write back `VIRTIO_BLK_S_UNSUPP` |
| F-05 | Construction panic | Capacity is not a multiple of 512 | Panic at startup | Device cannot be created | 2 | `RamDiskBackend::new` asserts on the configuration error (a configuration mistake by the trusted caller) |

## Failure Management

- Guest-controllable failures are always returned via `VirtioResult` and status codes, without panicking.
- The only panic point is `RamDiskBackend::new`: the capacity must be a positive multiple of 512 bytes. A violation is a caller configuration error (trusted input), not guest-controllable.
- A length mismatch in backend `read`/`write` returns `BackendError`, and the request completes with IOERR.

## Privacy Analysis

This crate does not process private user data; it only moves bytes between the guest RAM disk and guest descriptor buffers.
`selftest` uses only fixed test data and no real user data.

## Known Limitations

- The 3 test cases in `ut-tests/device.rs` run in the kernel harness and use only mock
  memory owned by the test itself and a synchronous backend. The fixed MMIO addresses are model inputs; they do not map a real device and do not register an IRQ;
  kernel lock guards still perform the normal IRQ/preempt protection. These test cases do not cover a real VM or DMA.
- The default backend is an in-memory RAM disk and is not persistent.
- Advanced features beyond `VIRTIO_BLK_F_*` (discard/write-zeroes, etc.) are not implemented.
- MMIO transport only; PCI is not supported.
- The default base/IRQ is fixed to QEMU `virt` slot 1; multiple devices or a custom layout require extending the constructor parameters.

## Audit Checklist

- [ ] Do all newly added request types go through direction and capacity validation?
- [ ] Are all guest lengths constrained by `seg_max`/`size_max`/capacity before allocation?
- [ ] Does buffer allocation keep returning an error on `try_reserve_exact` failure?
- [ ] Does the reset path clear all pending intermediate state?
- [ ] Does a newly added backend declare the correct `requires_deferred_processing` and provide a polling path?
- [ ] Is interrupt injection invoked outside the device lock?
