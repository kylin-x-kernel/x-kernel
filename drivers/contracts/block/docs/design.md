# block — Design

## Purpose

`block` is the X-Kernel block core. It defines driver-private I/O operations,
`Gendisk`, `RequestQueue`/`QueueLimits`, and the `BlockDevice` identified by
`dev_t`. It owns the single resident block-device registry.

## Linux Object Mapping

| Linux | X-Kernel | Ownership |
|---|---|---|
| `struct block_device_operations` | `BlockDeviceOperations` | Driver/backend algorithm, open-mode callback |
| `struct queue_limits` | `QueueLimits` | Logical/physical block size, alignment, minimum/optimal I/O size |
| `struct backing_dev_info` | `BackingDevInfo` | Disk-owned shared readahead budget; file history stays in KVFS |
| `struct request_queue` | `RequestQueue` | Canonical queue limits; request dispatch is added in a later phase |
| `struct gendisk` | `Gendisk` | Disk identity, queue, `part0`, state, operations |
| `struct block_device` | `BlockDevice` | `dev_t`, queue, sector range, capacity |
| `bd_holder` / exclusive `bdev_open` | `BlockDeviceClaim` | Exclusive holder lifetime |
| `add_disk` / `del_gendisk` | Same-named functions | Explicit publication and withdrawal |
| `blkdev_get_no_open` | `lookup_block_device` | Canonical `dev_t` lookup |
| `set_capacity` | `BlockDevice::set_capacity` | Mutable media capacity publication |
| `set_disk_ro` / `get_disk_ro` | `BlockDevice::set_disk_read_only` / `is_read_only` | Canonical disk state |

`BlockDeviceOperations` does not inherit the generic `Device`, because disk
identity belongs to `Gendisk`. The backend expresses I/O and the open, release,
and ioctl callbacks corresponding to Linux block-device operations. `Gendisk`
holds that algorithm object and the `RequestQueue`, but does not implement the
operations trait. Backend calls go through canonical `BlockDevice` methods so
capacity, bounds, and read-only policy cannot be bypassed. Until the kext4 Bio
migration in PR5, the legacy synchronous operations implementation on
`BlockDevice` only derives a block count and delegates to those methods; it
stores no second copy of state. Disk identity, queue, state, and operations
belong directly to `Gendisk`, without a separate disk-state object.
`BlockOpenMode` corresponds to Linux `blk_mode_t` and carries the KVFS opened-file
mode into open/ioctl. Concrete drivers such as loop do not define separate open
semantics.

Only whole-disk `part0` is created today. `BlockDevice` expresses view bounds as
`start_sector + num_sectors`, using fixed 512-byte sectors consistent with Linux
`sector_t`. A future partition scan can publish additional views without
introducing another kind of device object.

## Publication And Lookup

`Gendisk::alloc` creates `QueueLimits` and `RequestQueue` from the logical block
size reported by the backend, converts the backend block count to sector
capacity, and constructs canonical `part0` at the same time. Unpublished disks
can therefore be used for tests and internal assembly without registry access.
Driver probe subsequently calls `add_disk` through the block class lifecycle.
Publication only checks that the major is nonzero and the complete minor range
does not overlap another range under that major, then inserts the same
disk/part0 identity into the registry without reconstructing it. Devfs, KVFS
block-special open, filesystem mount, and boot root selection all use this
registry.

The registry is the single `Mutex<BTreeMap<DeviceNumber, GendiskRef>>`
(`BLOCK_DEVICES`). `add_disk`, `del_gendisk`, `lookup_block_device`, and
`block_devices` hold its lock during insertion, removal, and lookup. The lock
protects only the map; read-only state, claims, and capacity use the objects'
own synchronization primitives. No driver I/O or callback runs under the
registry lock.

`del_gendisk` removes the registry's owning disk reference by part0 `dev_t`.
Existing `Arc<BlockDevice>` references retain the complete `Gendisk`, and
`disk()` continues to return that same object. New lookups no longer find the
withdrawn disk. New media must use a new `Gendisk` even when reusing the same
`dev_t`, yielding a new canonical `BlockDevice` identity. Republishing the same
`Gendisk` retains its original `part0`.

## Ownership And Final Release

Linux `disk_to_dev()` in `include/linux/blkdev.h` uses `part0->bd_device`: the
whole block device provides the gendisk reference count.
`block/bdev.c:bdev_free_inode()` frees the gendisk when freeing the whole device.
The ownership model here follows the same lifetime:

```text
GendiskRef / registry ──strong──> canonical part0 (BlockDevice)
                                        │
                                      strong
                                        ▼
                                     Gendisk
                                        │
                                      weak
                                        └────────> same part0
```

`GendiskRef` wraps only an `Arc<BlockDevice>` and borrows the complete disk through
`Deref`. It stores no identity, capacity, queue, open mode, or holder state. It
replaces a public `Arc<Gendisk>` and corresponds to acquiring/releasing a Linux
disk reference through `disk_to_dev()`. This owning reference is necessary:
exposing the internal `Arc<Gendisk>` would allow callers to retain the disk while
releasing part0, breaking the canonical whole-device lifetime.

`Gendisk::alloc` returns `GendiskRef` only after `Arc::new_cyclic` establishes all
relationships. The internal `Arc<Gendisk>` is not exposed;
`BlockDevice::disk()` returns only a borrow. Every disk borrow is therefore kept
alive by a live part0, and the weak upgrade in `Gendisk::part0()` must succeed;
failure is not a recoverable missing-device condition. Once the final owning
disk reference, device reference, or claim is released, part0 releases the disk.
The disk's weak back-reference does not form a strong cycle. Construction,
borrowing, and reclamation require no unsafe code.

Only whole devices are constructed today. Future partition ownership must also
keep canonical part0 alive; it must not expose the internal disk Arc or allow an
independently constructed `Gendisk` to escape.

## Exclusive Claims

`BlockDevice::claim_exclusive()` returns the RAII `BlockDeviceClaim`, corresponding
to Linux block-holder ownership. A canonical device admits one holder at a time.
The filesystem superblock holds that token directly and releases it on
initialization failure or when final shutdown enters the dead state. Different
filesystem instances cannot own the same media concurrently, and the block core
needs no knowledge of VFS or filesystem types.

## I/O Boundary

`BlockCompletionOperations` expresses device completion reclamation. Each call
must be finite, non-sleeping, and must not submit new requests. The host guarantees
serial callback execution within one registration; activation keeps a strong
reference to the target until synchronous stop completes. Each target is
registered at most once. The host owns queuing and stopping, without exposing
kernel APIs for these operations to drivers.

The `completion` module defines the OS-neutral `PrepareBlockWait`, `BlockWaiter`,
and `BlockSignals`. They do not change `BlockDeviceOperations` or directly
introduce kernel scheduler dependencies. The host injects an ordinary preparation
function returning a task-bound waiter. `waiter.signals()` exports an Arc to the
same notification state. Drivers use these contracts to wait for admission hints
and final completion. An admission hint does not grant submission rights;
queue/FIFO predicates and request results remain owned by the concrete transaction
implementation. Terminal notification must be published only after the device
has released the data buffers.

A waiter is not shared across tasks. Signals can be called from IRQ context and
retain no request, data, or device pointers. Waiting-resource preparation and
admission registration may return errors, but the terminal wait after submission
does not register again or return wait errors. The VirtIO implementation lives in
`virtio::blk`; `kdriver` supplies waiting and IRQ/softirq integration. Other drivers
can adopt these contracts without filesystem changes.

`QueueLimits` is the block core's sole source of device geometry. Backends
currently report only logical block size, so physical block size and minimum I/O
size default to logical block size, following Linux defaults. Optimal I/O size
and alignment offset are zero. Logical block size must be at least 512 bytes,
a power of two, and representable as `u32`. It is also bounded by the Linux
`BLK_MAX_BLOCK_SIZE` limit corresponding to the current 4-KiB page size.

Before delegating to the backend, `BlockDevice` checks that the buffer length is
a multiple of the logical block size, converts the legacy backend block ID to a
sector extent, checks that the entire extent fits within `num_sectors`, and
converts the sector offset including `start_sector` back to a backend block ID.
All multiplications and additions use checked arithmetic. `Gendisk::alloc`
converts the initial backend block count to sectors; `set_capacity` accepts a
sector count, as Linux does. Both paths require `num_sectors * 512` to fit in
`u64`. Capacity consumers read `num_sectors` or `size()` directly. The legacy
backend block count is converted once at construction and is not retained as a
second device-capacity state.

The backend reports its immutable, device-lifetime inherent read-only capability
through `BlockDeviceOperations::is_inherently_read_only()`. `Gendisk::alloc`
records it before publication. The administrative read-only state controlled by
`BLKROSET` is stored separately in `Gendisk` atomic state. Effective read-only
state is the union of inherent and administrative read-only state. Consequently,
`BLKROSET 0` succeeds after clearing only the administrative state; it cannot make inherently
read-only media writable. `BlockDevice::write_block` rejects writes to effectively
read-only devices before entering the backend.

KVFS owns the byte-level adaptation corresponding to Linux `blkdev_read_iter` /
`blkdev_write_iter`. Fully aligned blocks use the caller's buffer directly, while
leading and trailing partial blocks reuse one read-modify-write scratch buffer.
An ordinary write is not a durability barrier; only explicit `fsync` calls the
backend `flush`.

## Synchronous Buffer Address Contract

`BlockDeviceOperations::read_block` and `write_block` accept a buffer beginning
at any byte address. Whole-block lengths, range checks and read-only policy
remain in force. The same contract applies when callers hold a backend trait
object directly, through `Arc`, or through the canonical `BlockDevice`; putting
a bounce buffer only around the canonical view would not cover direct calls.
Buffer address alignment is distinct from request/block alignment.

Each backend and its existing DMA provider owns hardware address alignment,
contiguous-memory requirements, mapping and cache maintenance. The filesystem
must not carry a list of driver-specific address restrictions. Synchronous
callbacks must complete access to borrowed buffers before return.

`word_io` supplies the CPU-address adapter for a backend that requires u32
alignment. It validates block geometry, whole-block length and the complete
device range before any transfer. An aligned, nonempty buffer goes directly to
the callback, with no allocation or additional copy. An unaligned request uses
one initialized, provider-owned `CpuBuffer` allocation; each successful read chunk is
copied out, or each write chunk is copied in before submission. One buffer is
reused for at most 64 KiB at a time, except that a larger device block requires
one complete block. Allocation happens before the callback takes a device lock.
Staging requests ordinary aligned CPU memory through `CpuMemoryOp`. The host
adapter uses the ordinary allocator's inherent fallible entry, not the fatal
`GlobalAlloc` entry that can abort even through Vec's try-reserve API. The
initialized `CpuBuffer` owner releases through its original provider. There is
no test-only storage variant or factory; tests inject providers through the
same public path. CPU staging creates no DMA mapping or bus address and does
not establish cache coherency. Before submitting DMA, a backend must establish
a supported mapping/cache protocol for both direct and staged buffers. Naming
that responsibility does not prove that a legacy HAL implements it.

The pinned AHCI SDK inserts `Hal::virt_to_phys` results for data buffers into
its PRDT and does not call a data-buffer mapping API. The host's SDK
`flush_dcache` callback is only an ordering barrier, not data-cache maintenance.
Consequently `AhciHal` now explicitly certifies hardware-coherent DMA through
`DMA_COHERENT`; `AhciDriver::new` rejects false before entering the SDK. The
host only opts in on x86_64. Non-coherent AHCI remains unsupported, including
its command allocations: making only the unaligned scratch uncached would not
fix cached aligned requests or command memory. Enabling it requires an actual
SDK/HAL mapping/cache protocol, not changing the capability flag.

BCM's pinned `read_block`/`write_block` path instead copies CPU words through
the DATA FIFO. That PIO path keeps ordinary CPU staging; it has no data-buffer
DMA mapping for a streaming pool or coherent allocator to replace.

The existing streaming-DMA bounce pool is not a CPU scratch allocator: mapping
registers device-visible addresses and unmapping performs direction-dependent
copy-back and pool release. Using it here would introduce an unused DMA mapping
and a second lifetime protocol. CPU staging instead acquires one ordinary
buffer per unaligned request and reuses it across that request's chunks. No
cross-request cache or measured performance improvement is claimed.

Errors are returned without guessing whether an `InvalidInput` means alignment
and without retrying a transfer. Allocation failure performs no I/O. A failed
read chunk is not copied from the bounce buffer, but earlier successful chunks
may already have changed the caller's output; aligned I/O can modify the output
before failing. Writes may have changed earlier media blocks. Neither path
promises atomic failure, a successful prefix result, or implicit durability.

Legacy production backends remain disabled in the feature/dependency
configuration. Kernel UT compiles the actual BCM and AHCI adapters against CPU
controller fixtures. BCM exercises its word views, geometry guards, per-sector
loop and CPU provider; AHCI exercises admission before SDK entry, direct
forwarding and staged success/error/release paths. This does not enable either
SDK, probe hardware or validate hardware DMA/cache handling. Keep one module
declaration per adapter: production re-enablement must extend its existing
`cfg(unittest)` predicate instead of appending another declaration. The absent
Cargo features/dependencies remain commented. See [the re-enablement checklist](legacy-backend-checklist.md)
for the remaining SDK, integration and hardware obligations.

The [backend inventory](backend-buffer-contract.md) records each concrete and
test-only trait implementation's address handling and its validation limits.
It distinguishes caller-address checks from length/offset checks and static
backing-store alignment; source review alone does not certify hardware.

## Generic Ioctls And Disk Sequence

`BlockDevice::ioctl` handles generic block commands centrally. Geometry comes from
queue limits, capacity from the sector view, and the disk sequence from
`Gendisk::diskseq`. KVFS supplies fault-checked copy-in/copy-out and the current caller's `has_sys_admin` capability value. Only unknown
commands reach the driver; errors from generic commands do not trigger fallback.
`BLKROSET` accepts any nonzero int as read-only. The block dispatcher checks that capability before copying its argument.
The POSIX entry point passes the current caller's credential snapshot through KVFS,
which calls `Cred::has_sys_admin()` (currently effective UID 0) for `CAP_SYS_ADMIN`. Driver dispatch
receives the same capability value and open mode for its own command checks.
Readahead ioctls read and update `Gendisk::bdi` (`BackingDevInfo::ra_pages`).
Setters take an immediate sector count and round down to base pages; getters
copy a native unsigned long sector count, rejecting null before copying.
KVFS snapshots this budget when opening a file on the disk-backed superblock,
as Linux `file_ra_state_init` does. Existing opens keep their own history and
budget. Device-less files use the 128 KiB default. Loop owns no duplicate policy.

Disk allocation and successful loop SET_FD/CLR_FD operations assign a new global
sequence number; republication and capacity changes leave it unchanged. Drivers
call `inc_diskseq` under their existing media-change locks. Relaxed atomics are
sufficient because the sequence only identifies media generations. It does not
replace I/O synchronization, cache invalidation, or hot removal, and duplicates
no geometry or capacity state.

## Non-Responsibilities

- No device I/O execution: reading, writing, and flushing media belong
  to the concrete backend behind `BlockDeviceOperations` (for example
  `virtio::blk`); the core only validates and routes.
- No IRQ wiring or completion threading: waiting, admission, and
  IRQ/softirq delivery are owned by `kdriver`'s block host providers;
  this crate defines the contracts only.
- No page-cache algorithm or cache ownership: the disk owns the shared
  readahead budget; file windows, cached pages, byte-level adaptation, durability
  semantics, and mount ownership live in KVFS and the filesystem layer.
- No partition scanning: only whole-disk `part0` is published today.
- No media encryption or access control beyond the read-only and
  exclusive-claim rules documented here.

## IRQ Completion Requirements

These frozen requirements describe caller-visible behavior, architecture
constraints (08-09), and delivery obligations (11), not a particular completion
mechanism. Sources are the user's blk_irq scope decisions, the block API at
baseline 80574592, and the repository driver-boundary rules. PR 727 supplied
problem context only; its implementation is not a design or code input.

Scope: ordinary reads, writes and flushes through filesystem/block interfaces;
virtio-blk is the first converted driver. The synchronous API remains unchanged.
Automatic polling fallback, conversion of every driver, forced DMA cancellation,
and unrelated scheduler/preemption repairs are not included. IRQ setup failure
fails activation without publishing a disk (accepted integration policy).

An accepted request has passed block validation and been accepted for device
processing. Continuous polling means repeatedly checking device completion for
the duration of outstanding I/O instead of waiting for a completion event.

### REQ-BLKIRQ-01: Preserve caller-visible block operation semantics.

- Source: current block API and user clarification.
- Behavior: ordinary callers continue to observe synchronous read, write, and
  flush operations. Success means the requested data transfer or flush effect
  required by the existing block contract has completed before the call returns.
- Acceptance: existing filesystem and block-device call sites compile without
  API changes; existing block read/write/flush behavior tests remain valid.

### REQ-BLKIRQ-02: Keep ordinary filesystem block I/O on the general block path.

- Source: user clarification and current X-Kernel call paths.
- Behavior: ordinary filesystem block reads, writes, and flushes reach concrete
  block drivers only through the normal block interfaces used by the rest of the
  system. Filesystems do not need to know that virtio-blk is the backend in
  order to benefit from interrupt-driven completion.
- Acceptance: code review shows filesystem and block-device-file paths do not
  call virtio-blk-specific APIs for ordinary block I/O.

### REQ-BLKIRQ-03: Complete virtio-blk requests without continuous polling.

- Source: user clarification and current virtio-blk polling behavior.
- Behavior: for an accepted virtio-blk read, write, or flush on an IRQ-capable
  device, the waiting caller does not continuously poll device completion state
  until the request finishes.
- Acceptance: test, trace, or instrumentation demonstrates an accepted
  virtio-blk request whose completion is observed after device interrupt
  notification and whose waiting caller is not on the continuous polling path.

### REQ-BLKIRQ-04: Return exactly one result for each accepted request.

- Source: synchronous block API expectation and PR review discussion about
  completion races.
- Behavior: each accepted read, write, or flush returns one completion result to
  its original caller. The result is not duplicated or lost when multiple
  requests or multiple block devices complete close together.
- Acceptance: tests or review evidence cover multiple completions and show every
  accepted request returns exactly once.

### REQ-BLKIRQ-05: Do not lose completion across wait races.

- Source: PR review discussion and requirement for accepted requests to finish
  visibly.
- Behavior: if a request completes before, during, or after the caller begins
  waiting for it, the caller can still observe the completion result.
- Acceptance: tests or reviewed interleavings cover completion-before-wait,
  wait-before-completion, and concurrent completion/wait setup.

### REQ-BLKIRQ-06: Preserve validation and read-only outcomes.

- Source: current block contract and user requirement that FS I/O remains on the
  general block path.
- Behavior: invalid block size or extent requests are rejected according to
  existing block behavior. Writes rejected because the effective block device is
  read-only are observed as `DriverError::ReadOnly`.
- Acceptance: existing block validation and read-only tests remain valid; tests
  or review evidence show the IRQ-capable virtio-blk path cannot change these
  caller-visible outcomes.

### REQ-BLKIRQ-07: Preserve caller-visible error meaning.

- Source: current `DriverError` contract and PR review discussion.
- Behavior: ordinary device I/O failures are reported as I/O failures.
  Temporary inability to accept more device work is reported as a retryable or
  backpressure condition, not as internal state corruption.
- Acceptance: tests or review evidence cover ordinary device failure and
  queue/backpressure failure mapping.

### REQ-BLKIRQ-08: Keep the new completion behavior reusable for future block drivers.

- Source: user clarification and Linux reference.
- Behavior: adding interrupt-driven completion for virtio-blk does not require
  filesystem-facing changes that would have to be repeated for another
  IRQ-capable block driver.
- Acceptance: design review can trace how another concrete block driver would
  expose the same caller-visible completion behavior without filesystem changes.

### REQ-BLKIRQ-09: Preserve reusable-driver portability boundaries.

- Source: project driver boundary rule.
- Behavior: reusable concrete driver crates remain independent from X-Kernel
  host-kernel implementation crates for host-specific capabilities.
- Acceptance: dependency metadata and imports show no new direct dependency from
  `drivers/devices/virtio` to host crates such as `kirq`, `ktask`, `kwork`,
  `khal`, `memspace`, `kdma`, or `kruntime`.

### REQ-BLKIRQ-10: Demonstrate reduced CPU waste under delayed I/O.

- Source: user focus on replacing polling with IRQ handling and PR performance
  motivation.
- Behavior: under an I/O-latency scenario, a task waiting for virtio-blk I/O
  should not consume CPU as if it were continuously polling for the full wait.
- Acceptance: runtime evidence, benchmark evidence, or instrumentation compares
  the new path with current polling under a delayed-I/O workload. If the
  environment cannot provide stable evidence, the missing validation and reason
  are recorded.

### REQ-BLKIRQ-11: Keep records, docs, and tests aligned with accepted behavior.

- Source: project change-completeness rules and loaded collaboration skill.
- Behavior: task records, affected design/security docs, rustdoc for touched
  public APIs, and regression tests reflect the final accepted behavior.
- Acceptance: documents are updated where behavior or contracts changed; focused
  tests are added or updated; build/lint/test commands are run according to the
  project workflow or recorded as not run with reasons.

## Completion Integration

The synchronous caller owns its buffers until return. Concrete drivers own
hardware submission, request identity and buffer retirement; host providers own
waiting and deferred execution. `block` defines the shared contracts without
owning hardware tokens or scheduler objects. Filesystem callers use
`BlockDevice`/`Gendisk`, never a VirtIO-specific wait or interrupt API.

- [VirtIO transactions and resource lifetime](../../../devices/virtio/docs/design.md)
- [Host waiting, IRQ dispatch and removal](../../../integration/kdriver/docs/design.md)

## Staged Migration And Test Boundaries

PR1 consolidates disk lifetime, sector capacity, and queue geometry without
establishing another request-submission path. The existing virtio request-wait,
IRQ completion reclamation, and shutdown-drain contracts on main remain in
force. Subsequent Bio/Request migration should reuse and consolidate these
completion mechanisms instead of retaining parallel request-ownership and
completion states.

Constructing an unpublished disk lets tests avoid registering a global device;
it does not make an entire filesystem crate independently runnable in a host
process. Dependencies such as `block -> ksync -> ktask` and
`kext4 -> kvfs -> ksignal/memspace` still bring in the kernel runtime. Allocator,
linking, and runtime dependency issues with direct `cargo test -p kext4` require
an explicit distinction between test layers: pure disk-format and algorithm
tests can run on the host, while integration tests involving the real block
layer, VFS, scheduling, and page cache use the configured kernel unit-test path.
Supporting host tests for the same storage algorithms requires isolating runtime
dependencies at the existing capability and integration boundaries. Tests must
not introduce a parallel device model, fabricate kernel linker symbols, or claim
host-test success merely by skipping tests that cannot link.

## Kernel unit-test layout

The `ramdisk` and `ramdisk_static` modules require
`KFEAT_DRIVER_RAMDISK=y` and `KFEAT_DRIVER_RAMDISK_STATIC=y`, respectively.
For the static variant, supply the image path through `XKERNEL_RAMDISK_IMG`
as consumed by `build.rs`. When collecting QEMU unit-test coverage from
`disk.img`, set `KFEAT_ROOT_BLOCK="vda"` so the profile is written to the
persistent virtio disk rather than the embedded RAM disk.
The AHCI, BCM2835 SDHCI and SDMMC modules and their dependency declarations
remain commented out in the current crate. Their relocated tests are preserved
source, not registered or executed coverage; this layout migration does not
reactivate those backends.

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/bcm2835sdhci.rs`: mounted by `src/bcm2835sdhci.rs` as `tests_bcm2835sdhci`.
- `ut-tests/ahci.rs`: mounted by `src/ahci.rs` as `tests_ahci`.
- `ut-tests/ramdisk.rs`: mounted by `src/ramdisk.rs` as `tests_ramdisk`.
- `ut-tests/ramdisk_static.rs`: mounted by `src/ramdisk_static.rs` as `tests_ramdisk_static`.
- `ut-tests/lib.rs`: mounted by `src/lib.rs` as `tests`.
- `ut-tests/sdmmc.rs`: mounted by `src/sdmmc.rs` as `tests_sdmmc`.
