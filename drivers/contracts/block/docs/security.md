# block — Security And Reliability

## Scope

This analysis covers the block-core contract in `src/lib.rs`,
`src/completion.rs`, `src/backing_dev.rs`, and `src/ioctl.rs`. The concrete backends (`src/ahci.rs`,
`src/bcm2835sdhci.rs`, `src/sdmmc.rs`, `src/ramdisk*.rs`) are hardware
adapters excluded from this document and audited separately; the
registry, validation, and completion invariants below are the core's
own.

Core regression fixtures live in `ut-tests/lib.rs`; backend fixtures live
in the corresponding `ut-tests/*.rs` files and retain the backend scope
above. The static-ramdisk fixture's allocation and slice construction retain
their local SAFETY contracts. Kernel UT compiles AHCI and BCM2835 SDHCI with
CPU controller fixtures; production SDKs/probes and SDMMC remain disabled.

## Trust Boundaries

Driver-reported disk identity, initial capacity, logical block size, and I/O
completion cross the block-core boundary. External media contents are untrusted;
format validation belongs to the filesystem consuming the device.

## Invariants

- The major must be nonzero; the minor range must be nonempty and must not
  overflow or overlap an already-published disk.
- Logical block size must be a power of two in the range 512..=4096 bytes.
- `Gendisk` is associated with its unique `part0` and `RequestQueue` at
  construction; publication must not replace them.
- `GendiskRef` keeps the disk alive through part0. `BlockDevice` strongly owns
  the complete `Gendisk`; the internal disk Arc is not exposed.
- The disk holds only a weak reference to part0. Releasing the final external
  owning reference allows the device, disk, and backend to be fully reclaimed.
- Every borrowable disk has a live part0. `disk()` cannot return a missing-disk
  state, and the upgrade in `part0()` must succeed.
- `BlockDevice` start and capacity use 512-byte sectors; `num_sectors * 512`
  must be representable in `u64`.
- One `dev_t` maps to exactly one `BlockDevice` in the resident registry.
- A canonical `BlockDevice` has at most one exclusive holder at a time;
  the claim token owns release.
- Every I/O buffer length is a multiple of the logical block size, and the
  complete sector extent lies within current capacity.
- Buffer start addresses may be arbitrary. Device address, contiguous-memory,
  DMA mapping and cache-maintenance requirements belong to the backend and its
  provider, including when a caller bypasses the canonical view with a direct
  operations trait object.
- Block/sector conversions and offset/length multiplication and addition use
  checked arithmetic.
- The operations object, `Gendisk`, and `BlockDevice` are all `Send + Sync`.
- No driver I/O or callback runs while holding the registry lock.
- The backend's inherent read-only capability is recorded by `Gendisk` before
  publication and cannot be cleared through the administrative interface.
- Administrative read-only state belongs only to the owning `Gendisk`;
  effective read-only state is the union of inherent and administrative state.
- Every `BlockDevice` write checks effective read-only state before entering
  the backend.
- `Gendisk` does not implement the driver operations trait. The transitional
  synchronous trait implementation on `BlockDevice` delegates to canonical
  methods and cannot bypass capacity, bounds, or read-only checks.

## Unsafe

`drivers/contracts/block/src/lib.rs` contains no unsafe blocks. The MMIO/DMA
safety boundaries of concrete hardware backends belong to each backend's own
documentation.

`word_io` uses `device_res::CpuBuffer` for initialized ordinary CPU storage.
Its unsafe `CpuMemoryOp` implementation contract supplies unique storage of the
requested length/alignment; the RAII handle releases through its original
provider after every byte borrow ends. The block adapter contains no unsafe
memory conversion and has no alternate test-only storage model. Test providers
inject allocation failure and observe release at the same capability boundary.
CPU alignment alone does not certify a device's DMA/cache handling; staging
creates no DMA mapping, bus address, uncached mapping or platform-sharing hook.
The backend must establish its DMA/cache protocol before submission for both
aligned and staged buffers. The former statement of backend responsibility
must not be interpreted as evidence that a legacy HAL supplies cache sync.

The AHCI SDK translates data pointers directly into PRDT physical addresses;
the host `flush_dcache` callback is only `dma_read_barrier`, without data-range
clean/invalidate. The unsafe `AhciHal` contract now makes a coherent-DMA claim
explicit. A false `DMA_COHERENT` rejects initialization before SDK/MMIO/DMA
entry, including command initialization; the host reports true only on x86_64.
Its new CPU fixture tests make SDK entry on a non-coherent HAL fail
deterministically. Non-coherent AHCI support has not been implemented. Merely
making unaligned scratch uncached would leave aligned data and SDK command
allocations unprotected and is deliberately not treated as a fix.

BCM's pinned SDK moves data between the CPU word slice and the MMIO DATA FIFO.
For that PIO path, device DMA never accesses ordinary CPU scratch. Its cache
attributes therefore do not require coherent DMA allocation.

The adapter validates geometry and the complete range before I/O. One owned
buffer, bounded by 64 KiB or one device block, is allocated fallibly before a
device lock and is reclaimed on every return path. Already aligned requests
allocate no buffer. Allocation failure returns `NoMemory` without invoking the
device. Production allocation uses the fallible CPU-memory capability rather
than the host GlobalAlloc OOM policy. Failed chunks are never guessed to be
alignment errors or retried.
Completed chunks may have side effects, so the adapter supplies no rollback or
failure-atomicity guarantee. Disabled legacy backends require separate dependency
and hardware validation before re-enablement.

Under `cfg(unittest)`, the actual BCM adapter is compiled with a CPU controller
fixture. Its `align_to[_mut]::<u32>` views rely on word_io's four-byte alignment,
initialized owned/borrowed storage and whole-sector lengths. Runtime guards
reject prefix/suffix residues and incompatible SDK block geometry. Tests cover
unaligned multi-sector I/O, direct aligned borrowing, failure isolation, release
and all error mappings. The fixture replaces only the controller, not the
adapter's unsafe views. It supplies no MMIO/DMA/hardware evidence; production
SDK/probe paths stay disabled. The explicit remaining checks are listed in
[legacy-backend-checklist.md](legacy-backend-checklist.md).

## Failure Handling

`BlockCompletionOperations::process_completed_requests` is a trusted driver
callback and must be finite and non-sleeping. The host's serial-execution
guarantee applies within one registration; creating multiple registrations for
the same target must not be assumed to preserve mutual exclusion. Completion
callbacks must not wake callers while holding a device lock. A strong reference
to the target must span synchronous stop before device resources are destroyed.

The completion-wait contract requires provider signals to own only notification
state, never requests or DMA data. Callers notify outside device/registry locks.
A `BlockWaiter` is used only by the task that prepared it, in sleepable context.
A failed admission wait cancels the pending node at the transaction layer and
passes on the admission opportunity. The terminal wait has no recoverable-error
exit, preventing a request still involved in DMA from being freed early because
of a wait error. The contract itself implements neither queues nor DMA lifecycle
and does not verify device completion; drivers must publish the real result
before raising terminal notification.

| Failure | Result |
|---|---|
| Identity/range conflict | `AlreadyExists`; no partial disk is published |
| Invalid logical block size or initial sector capacity | `InvalidInput`; no partial object is constructed |
| Existing exclusive holder | `ResourceBusy`; no second holder is created |
| Invalid length, block geometry, or range | `InvalidInput`; backend not called |
| Unaligned buffer address | Backend adapts it; address alone is not an error |
| Bounce allocation failure | `NoMemory`; no device I/O |
| Backend I/O/flush failure | `DriverError` propagated unchanged |
| Write to a read-only disk | `ReadOnly`; KVFS maps it to Linux `EPERM` |
| `BLKROSET 0` on an inherently read-only disk | Success; administrative state is cleared, inherent read-only remains effective |
| Unknown disk-specific ioctl | `NotATty` |
| New open/mount after disk withdrawal | Canonical lookup fails |

## Ioctl Boundary

Generic ioctls copy values using their actual Linux ABI widths, not the size
bits in the command encoding. Copy faults propagate unchanged. A failed
`BLKROSET` argument read does not change state, and unknown commands do not access
arguments before driver dispatch. The block dispatcher checks administrative
authorization using the host-supplied `has_sys_admin` value before copy-in; the current effective-UID-zero check does not
implement the full capability/user-namespace model. The disk sequence is neither
an access token nor an I/O barrier, and queries do not provide an atomic state
snapshot across multiple ioctls.

The trusted host must derive the capability value from the current caller;
passing a boolean does not authenticate an untrusted kernel caller. Command
policy belongs to the generic dispatcher or the driver owning that command.
Linux `drivers/block/loop.c::lo_ioctl` does not require CAP_SYS_ADMIN or an owner
check for SET_FD/CLR_FD; admission relies on device-node access, and SET_FD derives
read-only mode from the device and backing-file open modes. SET_STATUS requires
write-open mode or CAP_SYS_ADMIN. X-Kernel follows those checks; its default
0666 device nodes therefore allow unprivileged loop attachment/detachment.
This is a development-stage permission policy, not Linux distribution policy;
production node ownership/modes must restrict device administration.
BLKRASET/BLKFRASET require CAP_SYS_ADMIN before updating the disk's shared
readahead budget. Native unsigned-long multiplication and page rounding follow
Linux; getters reject null with EINVAL and copy exactly a native unsigned long.
The budget is a default for future opens, not a cache allocation or a change to
already-open files.

## Known Limitations

Raw block-device mmap returns ENODEV: block-special inodes have empty
address-space operations, and no canonical block cache with a media-change
invalidation protocol exists yet. `inc_diskseq` is only an identity operation,
not an implementation of Linux `disk_force_media_change`. Enabling block mmap
requires that cache ownership and invalidation together; a devfs-inode-local
cache would not satisfy the contract.

There is no partition scan today; only whole-disk part0 is published. Hot removal
does not actively freeze already-mounted filesystems; further I/O through
existing references relies on the backend returning device errors.
