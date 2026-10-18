# Backend buffer-address contract inventory

The trait requires valid synchronous byte buffers to be accepted independently
of their starting address. It does not waive supported lengths, device ranges,
read-only state, buffer lifetimes, DMA/cache obligations or real resource/I/O
errors. This inventory separates source review from execution evidence.

## Concrete backends and forwarding

| Implementation | Address handling in the reviewed source | Validation boundary |
|---|---|---|
| `virtio::VirtIoBlkDev` | Both polling and IRQ request paths retain borrowed byte buffers. The virtqueue puts the HAL `share` result in its descriptor; the X-Kernel HAL returns the streaming mapping's bus address and reverses it with `unshare`. No caller-address alignment rejection or typed word view occurs. | The selected HAL must implement that mapping/lifetime protocol. A CPU-only mock HAL is not evidence for another platform HAL or real hardware. |
| `AhciDriver` | `word_io` passes word-aligned callers directly and otherwise stages through ordinary CPU memory. The SDK uses physical data addresses directly; its host barrier performs no data-cache maintenance. Initialization now rejects HALs without explicit hardware-coherent DMA admission, before entering the SDK. | The host only opts in on x86_64; non-coherent AHCI is unsupported. Kernel UT compiles the actual adapter with a CPU controller. Production feature/SDK/probe disabled; hardware `NOT_RUN`. |
| `SDHCIDriver` (BCM) | Same CPU adapter; guards SDK block geometry and consumes whole sectors via word views. The pinned SDK copies CPU words through the MMIO DATA FIFO (PIO), without data-buffer DMA. | Kernel UT compiles the actual adapter with a CPU controller fixture. Production feature/SDK/probe disabled; hardware `NOT_RUN`. |
| `SdMmcDriver` | `as_chunks[_mut]::<BLOCK_SIZE>` checks length and forms byte arrays, whose alignment is one. The pinned SDK copies FIFO word values to/from byte slices instead of casting the caller to words. No caller-address gate. | Disabled source path; SDK/hardware execution `NOT_RUN`. Separate pre-existing SDK panic/range/error concerns are not certified by this address review. |
| Heap `ramdisk::RamDisk` | `copy_from_slice` in both directions, with length/range checks; no caller-address gate. | Source review; no claim of a dedicated runtime run with this feature. |
| Static `ramdisk_static::RamDisk` | Byte copies in both directions. Constructor alignment applies to its static backing store, not read/write callers. | Source review; no claim of a dedicated runtime run with this feature. |
| `devfs::LoopDevice` | Computes file offsets and uses VFS byte read/write operations; checks complete returned length. No pointer-address gate. | Relies on the backing VFS file's byte-buffer contract. Dedicated unaligned loop runtime `NOT_RUN`. |
| `Arc<T>` forwarding | Forwards the identical slice to `T`; adds no address restriction. | Requires the selected backend to honor the trait, including direct calls. |
| Canonical `BlockDevice` forwarding | Canonical methods validate capacity/geometry/read-only state and forward the byte slice; no address gate. | Backend still owns hardware adaptation; no wrapper-only guarantee. |

The sdmmc SDK reviewed is `simple-sdmmc` revision `9e6420c`: `read_block` /
`write_block` take `[u8; 512]`; its FIFO read uses `to_le_bytes` plus byte-copy,
and FIFO write uses `from_le_bytes` on an eight-byte slice. MMIO word accesses
are accesses to controller registers, not casts of the caller buffer. Loop's
and sdmmc's length/offset checks must not be confused with address checks.

The VirtIO review follows `kvirtiodrivers` 0.13.4, its descriptor `set_buf`,
the X-Kernel HAL glue, and `device-res-xkernel`'s DMA provider. The host
streaming provider allocates from its DMA bounce pool, tracks the active
mapping and performs direction-dependent copy-back on unmap. Its device bus
address is used by VirtIO; that is different from pure CPU word staging.

## Test-only implementations (not hardware backends)

The remaining implementations were also reviewed for address assumptions:

| Source/type | Behavior |
|---|---|
| `block/ut-tests/lib.rs`: `MemoryDisk` | Byte copies with range checks. |
| Same file: `GeometryDisk` | Intentionally rejects I/O as unsupported; no address-based branch. |
| Same file: `InherentlyReadOnlyDisk` | Forwards to `MemoryDisk`, with read-only metadata for canonical tests. |
| `block/ut-tests/word_io.rs`: `RestrictedDevice` | Real CPU adapter around its deliberately word-restricted transfer callback. |
| `kext4/src/io.rs`: `MemoryBlockDevice` | Byte copies and recorded write lengths. |
| `kext4/src/file/read_tests.rs`: `ReadDevice` | Public CPU adapter precedes the restricted raw read callback and injected failure. |
| Same file: `ReadStorage` | Byte copies with checked ranges. |
| `kext4/src/buffer/tests.rs`: `TestDevice` | Byte copies with length/range checks and intentional faults/delays. |
| `kext4/src/extent/legacy_tests.rs`: `ReadOnlyDevice` | Byte-copy reads; deliberately unsupported writes. |
| `kext4/src/superblock/test_fixtures.rs`: `TestDevice` | Byte copies and injected write/flush errors. |
| `kext4/src/superblock/fault_device.rs`: `WriteThroughFaultDevice` | Byte copies with injected I/O errors. |
| Same file: `BufferedFaultDevice` | Byte-copy committed/pending ranges with fault injection. |
| `kvfs/src/node/device.rs`: `MemoryBlockDevice` | Byte copies, counters and deliberate partial failures. |
| `kvfs/src/super_block.rs`: `TestBlockDevice` | No-op I/O fixture for superblock publication/lifetime; not data-correctness evidence. |

No additional caller-address rejection was found among these implementations.
That source conclusion is not a claim that every backend/platform/length/error
combination has been run. In particular, legacy SDK/probe and hardware work
remains subject to [the re-enablement checklist](legacy-backend-checklist.md).
