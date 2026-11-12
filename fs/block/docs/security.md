# fs_block — Security and Reliability Analysis

## Trust Boundary

`SeekableDisk` receives an `Arc<BlockDevice>` that has already been established by the block core. The media contents, capacity, block
size, and I/O results are still untrusted; the on-disk format, features, and checksums are validated by the concrete filesystem.

This crate does not access user pointers, MMIO, PIO, or DMA, and it does not own device publication or pathname lookup.

## Memory and Concurrency Invariants

`fs/block/src` contains no `unsafe` blocks.

- The partial-block buffer length equals the block size; the block size must be a power of two;
- `offset` is always less than the block size, and resets to zero after crossing a block boundary;
- Mutable cursor operations require exclusive `&mut SeekableDisk`;
- A pending write is committed only on paths that can explicitly propagate errors.

## Fault Handling

| Fault | Result |
|---|---|
| block read/write/flush failure | Propagated unchanged to the filesystem adapter |
| Read-ahead failure of an unaligned partial write | The write buffer dirty state is left unmodified and an error is returned |

## Audit Checklist

- Has the device registry or `dev_t` identity been mistakenly put back into this crate?
- Does any new adapter swallow persistence errors in drop?
- Has the filesystem type or root mount policy been misplaced into this crate?

## Buffer allocation failure

Layout overflow, invalid flags and allocator failure propagate from KBox without
publishing a cursor or issuing device IO. Failure releases any first buffer and
the constructor's device reference. This does not release other device owners.
Allocation flags do not constrain later IO, imply NMI safety or guarantee success.

Slice allocation failures are tested in kalloc-types with a local allocator
seam. There is no injected second-buffer OOM or complete failed-mount test in
this adapter; sequential local ownership provides cleanup on early return.
