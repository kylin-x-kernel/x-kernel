# pipefs — Security and Reliability Analysis

## Trust Model

The pipefs type, operation tables, and boot wiring are trusted kernel code. The status flags are an `OpenFlags` constructed by the syscall
layer after it has completed its combination checks, and the credential is an `Arc<Cred>` that is stable for the duration of the call. This crate
keeps only `NONBLOCK`, and derives the read-only end and write-only end access modes on its own.

## External Boundaries / Attack Surface

- The user flags of pipe2, after being narrowed by the syscall layer, affect the nonblocking state of the two files.
- The fsuid/fsgid of the credential determine the owner of the pipe inode metadata.
- Subsequent user I/O, poll, ioctl, and resize go into `kvfs::pipe`; this crate does not touch user pointers.

This crate does not access MMIO/PIO, DMA, device/interrupt inputs, firmware, FFI, or architecture assembly.

## unsafe Code Inventory

`src/lib.rs` contains no `unsafe` block.

## Memory Safety Invariants

- `PipeFs::global()` may return only after the hidden mount has been completely published.
- Each `create_pipe_files()` call must create a unique inode; the same read/write pair must share that
  inode and the same `Arc<PipeObject>`.
- The inode private data, read-file private data, and write-file private data must not point to different pipe sessions.
- The dentry must be bound to the pipefs superblock and inherit the static default dentry operations.
- If creation of the write file fails, the read file must not be published; if the read clone fails, the local objects are reclaimed by `Arc`.

## Thread Safety

The global mount is read-only and is published through `Once`. Concurrent creation contends only on the internal synchronization of the KVFS allocator/registry;
different pipe sessions share no mutable state. The synchronization contract of a single pipe is implemented by `PipeObject`.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | A pipe wrongly reuses the anon-inode singleton | High | Creating the two endpoints with anon_inodefs | The standalone pipefs creates a unique FIFO inode on every creation |
| T-02 | The two files point to different pipe state | High | The private object is reallocated during clone | The inode/read/write explicitly clone the same `Arc<PipeObject>`, and tests check pointer consistency |
| T-03 | The inode owner is fixed to root | Medium | The calling credential is ignored | Inode init uses the passed-in `Cred::fsuid/fsgid()` |
| T-04 | An uninitialized pipefs is used by a syscall | High | Boot wiring is missing | `global()` panics, so the error fails explicitly at boot/test time |
| T-05 | The dentry name leaks a wrong identity | Low | A fixed `[pipe]` or an anon dname is used | The static pipefs dentry table generates `pipe:[ino]` from the unique inode number |
| T-06 | Pipe inode metadata is modified without DAC authorization | Medium | Bypassing the `Path` layer and going directly into the inode table | The inode uses `metadata_only_inode_operations()`, and metadata updates are published only after authorization through `Path::chown/chmod/set_times`. Size updates always return `EINVAL`. |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | pipefs is uninitialized | Wrong boot order | Pipe creation panics | Halt before userspace starts, or a test failure | 2 | `fs_boot` initializes it explicitly |
| F-02 | The caller passes unrelated status flags | The kernel call site does not narrow the arguments | Unrelated bits, if propagated, would pollute the file state | fd behavior deviates from pipe2 | 3 | The typed entry point keeps only `NONBLOCK`, and the access mode is derived fixedly by pipefs |
| F-03 | inode/file allocation fails | Out of memory | The current creation fails | Already allocated local state is released automatically | 3 | Propagated through `KResult`, and the two files are published in order |
| F-04 | The read clone fails | File allocation fails | The write file is not returned to the caller | The pipe state is reclaimed locally | 3 | The pair is returned only after the clone succeeds |

## Failure Management

Input and allocation errors are returned through `KResult`; being uninitialized is a violation of the boot contract and causes a panic. There is no retry path for half-initialized
global state, and failed pipes are not placed in any additional registry.

## Privacy Analysis

pipefs stores only the owner object of the pipe bytes, and neither reads nor records their contents. The dynamic name contains only the inode
number, and contains no pipe payload, credential, or user address.

## Known Limitations

- Linux notification pipes and `O_DIRECT` packet mode are not yet supported.
- The hidden pipefs does not support runtime unmounting.
- Pipe resource accounting/limits are still determined by the current `kvfs::pipe` capabilities.
- Updates to the owner/mode/times of a pipe inode are kept only in the resident `VfsInode` (pipefs has no backing storage,
  and likewise Linux pipefs does not persist them).

## Audit Checklist

- Are anonymous pipes created only through `pipefs::create_pipe_files()`?
- Does each creation allocate a unique inode, and do the read/write files share the inode and the `PipeObject`?
- Does the owner come from the calling credential, rather than a fixed root or a fresh read of the current task?
- Does the dentry display `pipe:[ino]`, and does the operation table come from the superblock default `s_d_op`?
- Does every new error path fully release the local file/inode/pipe state before returning an fd?
- Does the pipe inode still use `kvfs::metadata_only_inode_operations()`, with all metadata changes authorized by the `Path` layer?
