# entropy — Security and reliability

## Trust model and external boundaries

Hardware, firmware and VMM-produced bytes enter through source adapters. This
crate does not access user pointers, MMIO or DMA directly. VirtIO bytes depend
on host trust expressed by enabling `entropy_virtio_rng`; the Hwrng class itself
provides no entropy-quality proof.
The bootstrap seed is not sufficient to mark the pool ready. Secure consumers
must use the readiness contract before treating pool output as securely seeded.

## Memory and thread safety

The VirtIO collector contains no unsafe code. It gives a temporary stack buffer
to `read`; the Hwrng contract requires driver-owned storage for outstanding
DMA. A nonempty success is checked against the buffer capacity before mixing.
State transitions use one lock and all blocking reads and retry sleeps occur outside it. Prepared work submission
is nonblocking and is serialized with demand publication.
Pool mixing completes before readiness is published. Class handles retain Rust
objects, but do not establish hardware resource validity after hot removal.

## Threats and failures

| Failure or threat | Impact | Control and remaining limitation |
| --- | --- | --- |
| Untrusted VMM supplies predictable bytes | Compromised entropy quality | Disable `entropy_virtio_rng` when the host is untrusted; no statistical quality guarantee |
| Hardware never completes | Collector remains blocked | Occupies a shared worker and queue active allowance; no device timeout or reset |
| Read error without terminal device failure | Delayed seeding | Retain demand and sleep 100 ms before retry; log once per demand |
| Confirmed terminal device failure or invalid successful length | Hardware source becomes unavailable | Warn and permanently fail the instance |
| Repeated demands | Excess work/threads | One reusable Work with serialized callbacks and one coalesced pending demand |
| Device removed while a handle survives | Resource lifetime hazard | Hot removal/rebinding unsupported; driver/host must retain resources |

## Privacy and audit

Warnings contain errors or lengths, never random bytes. Review trust gating,
readiness publication, buffer-length validation, work reuse, failure logging,
and absence of blocking calls under the source spinlock. These tests establish
functional behavior, not randomness quality or a hard IRQ latency bound.
