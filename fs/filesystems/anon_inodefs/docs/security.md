# anon_inodefs — Security and Reliability Analysis

## Trust Model

The filesystem type, operation tables, and boot initialization code are trusted kernel code. The caller-provided
file operation object, typed private object, name, flags, and credential all already reside in the kernel
address space, but the name and flags still must not be treated as a source of filesystem structure or permissions.

## External Boundary / Attack Surface

- `name` enters the proc-style dynamic path display, but is only formatted as a kernel-owned Rust `str`.
- Raw syscall flags do not enter this crate directly; the caller passes in typed `OpenFlags`, and this crate only retains
  the access mode and nonblocking bits.
- `Cred` is only stored as the open-time credential; no pathname DAC is performed here.
- The caller-provided `FileOperations` may fail or block in the file open/release/I/O paths.

This crate does not access user pointers, MMIO/PIO, DMA, device input, firmware, FFI, or assembly.

## unsafe Code Inventory

`src/lib.rs` contains no `unsafe` block.

## Memory Safety Invariants

- `AnonInodeFs::global()` may return only after `Once` has completely published the mount and both shared inodes.
- All ordinary anonymous files must reference the same untyped shared inode, and all sockets the socket-typed one; no parallel
  inode identity is created for either kind.
- File private data is held by an `Arc` and must satisfy `Any + Send + Sync + 'static`.
- Dentry operations are inherited from the superblock's static default table and must not be replaced once the dentry is bound.
- The dynamic name reads only the dentry's own stable name snapshot.

## Thread Safety

Initialization is expected to be called serially by the boot CPU, and `Once` still guarantees idempotence on repeated calls. After publication, object fields are read-only;
concurrent file creation is managed by KVFS internal locks and `Arc`, and this crate has no mutable global state of its own.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Uninitialized access hides the mount | High | boot omits `init_anon_inodefs()` | `global()` panics immediately, exposing the ordering error during boot/testing |
| T-02 | Each file allocates its own inode or operation state | Medium | The caller bypasses the shared inodes or the filesystem stores a per-file table | Both public creation entry points (`get_file()`, `get_socket_file()`) bind one of the two shared inodes; operation tables are static/shared objects |
| T-03 | Unsupported open flags leak into file state | Medium | The caller mixes in create/path/exec flags | `get_file()` and `get_socket_file()` both retain only the access mode (`O_ACCMODE`) and `NONBLOCK`; neither masks less than the other |
| T-04 | Dentry operations are inconsistent with the superblock | Medium | `d_op` is manually installed per file | The dentry automatically inherits from the counterpart of the superblock `s_d_op` and cannot be replaced once bound |
| T-05 | The shared anonymous inodes' metadata (the untyped singleton and the socket-typed inode) is rewritten by a single caller across processes | Medium | A privileged credential holding any anonymous fd calls `fchown(at)`/`fchmod`/`futimens` | `AnonInodeOperations` explicitly overrides `setattr`: owner/mode/times keep the rejection semantics (`EOPNOTSUPP`) and do not inherit the KVFS common fallback layer; `ftruncate` keeps `EINVAL` for non-regular files; tests cover rejection of each field and that resident attributes remain unchanged |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Global object uninitialized | boot wiring is missing | The current creation panics | Stops before userspace starts | 2 | boot initializes explicitly; unit tests cover creation after initialization |
| F-02 | File allocation fails | Out of memory or VFS validation failure | The current creation returns an error | No global state change | 3 | Propagated unchanged through `VfsResult`; private data is installed only after success |
| F-03 | File release fails | Caller fops cleanup error | The current close returns an error | The filesystem singleton remains usable | 3 | Cleanup belongs to the caller's operation table; the shared mount/inode is not torn down with a single file |

## Fault Management

Recoverable file allocation and open errors are returned through `VfsResult`. A panic occurs only when the boot initialization contract is violated;
no implicit recovery or reconstruction of global VFS objects is attempted on first runtime access.

## Privacy Analysis

This crate does not read the contents of the file private object and does not copy user data. The dynamic name may appear in proc/debug
path output, so callers should use a fixed kernel-object class name and must not embed sensitive data.

## Known Limitations

- The Linux secure/unique anonymous inode and LSM security-init paths are not yet implemented.
- The hidden filesystem does not support runtime unmounting.
- Operation-table storage still follows the KVFS inode/file operation API; this crate establishes no additional cache.

## Audit Checklist

- Does the new anonymous kernel object use `AnonInodeFs::global().get_file()` (or `get_socket_file()` when the object must `fstat` as a socket)?
- Does boot call `init_anon_inodefs()` before any concurrent caller?
- Is the name a fixed class label that contains no user secrets?
- Are new flags typed at the syscall boundary, without bypassing the mask in `get_file()`/`get_socket_file()`?
- Is a per-file inode, mount, or dentry-operation owner added by mistake?
